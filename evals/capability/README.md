# Lato capability baseline

This is a **native macOS/Python adaptation** of 20 real issues from
[SWE-bench Verified](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified).
It is not the official Docker harness or a leaderboard submission. Selection is
purposive (10 SymPy, 6 pytest, 3 Sphinx, 1 Flask), so results describe this issue
repair sample, not all languages, feature development, or agent security.

The frozen manifest uses `openai-codex/gpt-5.6-luna`, three independent trials,
a 600-second agent deadline, and a 180-second test deadline. The model is a
provider alias; this runner cannot pin an undisclosed backend model revision.

## Run

Requires Python 3.11, uv, Git, an installed `lato`, its existing configured model
credentials, and a working Lato workspace sandbox. Credentials are never copied.
The run directory must be outside the source repository and is created mode 0700.

```sh
python3 -m unittest discover -s evals/capability -p 'test_*.py'
python3 evals/capability/runner.py prepare --root /absolute/path/to/private-run
python3 evals/capability/runner.py run --root /absolute/path/to/private-run
python3 evals/capability/runner.py report --root /absolute/path/to/private-run
python3 evals/capability/analyze.py --root /absolute/path/to/private-run --output /absolute/path/to/report.md
```

Use `--case INSTANCE_ID` (repeatable) for a subset. A completed preflight or trial
is reused; incomplete trial directories stop execution for inspection. Before
inference, environment-only preflight repairs may archive an invalid preflight
directory and re-run the same case. Do not change the model, tasks, prompt,
runner, collector, or binary during a run: the run fingerprint rejects changes.

## What gets checked

1. At the pinned base with upstream tests, all FAIL_TO_PASS cases must actually
   fail and all PASS_TO_PASS cases must pass. Missing, skipped, ambiguous or
   uncollected required tests cannot pass validation.
2. With the upstream reference fix, all those cases must pass. Invalid fixtures
   remain visible and are excluded from inference, not counted as model failures.
3. Lato receives the original problem, a uniform work instruction and local test
   command, in a new source snapshot with no upstream history. It does not receive
   the reference fix, hidden tests, hints or grader.
4. The full candidate diff includes newly created files and tests. A fresh grading
   workspace applies the candidate, restores upstream evaluation test files, then
   applies the evaluation test patch. Existing test/config changes are flagged.
5. Success requires all required tests, acceptable scope and a successful bounded
   agent execution. A fluent final answer is never sufficient.

Artifacts include the pinned archive hash, dependency lock, Lato binary hash and
dirty source diff, input prompt, combined stdout/stderr, session journal, complete
candidate diff, per-test outcomes, JSONL results and coverage-aware summary.
Tokens and monetary cost are null when Lato does not expose them. Journal matching
uses the unique workspace path; ambiguous attribution is reported explicitly.

The native workspace sandbox does not provide a hardened hidden-test or network
isolation boundary. The task instruction prohibits looking up solutions, but this
is not an adversarial security evaluation. Public benchmark contamination also
remains possible. Interpret results with those limitations.
