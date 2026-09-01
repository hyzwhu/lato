# Lato Agent Test Suite Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a source-auditable manual and automation-ready test suite for Lato, derived primarily from public agent benchmarks and supplemented only where Lato has product-specific behavior.

**Architecture:** A single test-case document groups cases by capability while attaching provenance to every case. Public benchmark tasks are paraphrased into locally reproducible fixtures; Lato-native cases are explicitly labeled and cover only CLI, provider, ACP, transcript, approval, and sandbox behaviors not represented by public benchmarks.

**Tech Stack:** Markdown, Git, pytest/cargo-test-compatible assertions, public benchmark metadata from Terminal-Bench, TUA-Bench, SWE-bench Verified, OpenHands Benchmarks, and AgentDojo.

## Global Constraints

- Do not present a self-authored case as an externally sourced benchmark case.
- Every sourced case must include benchmark name, original task ID or instance ID, and a direct source URL.
- Prefer outcome and workspace-state grading over natural-language answer matching.
- Separate offline, live-provider, capability, regression, and adversarial results.
- Do not include real API keys or private repository contents.
- Preserve the user's unrelated working-tree changes.

---

### Task 1: Build the provenance inventory

**Files:**
- Create: `docs/testing/lato-agent-test-cases.md`
- Reference: `/tmp/lato-agent-evals.EwSr60/terminal-bench/original-tasks/*/task.yaml`
- Reference: `/tmp/lato-agent-evals.EwSr60/tua-bench/tasks/*/instruction.md`
- Reference: `/tmp/lato-agent-evals.EwSr60/benchmarks/benchmarks/*`

**Interfaces:**
- Consumes: Public benchmark task statements, task IDs, graders, and licenses.
- Produces: A source inventory table used by every later section.

- [ ] **Step 1: Record benchmark-level sources**

Add the official repository or dataset URL, applicable task families, original grading style, and Lato applicability for Terminal-Bench, TUA-Bench, SWE-bench Verified, OpenHands Benchmarks, and AgentDojo.

- [ ] **Step 2: Define provenance labels**

Use exactly `Direct adaptation`, `Pattern adaptation`, and `Lato-native`. Explain that direct adaptations preserve the original outcome, pattern adaptations preserve the capability and grader shape, and Lato-native cases have no claimed external provenance.

- [ ] **Step 3: Audit the inventory**

Verify that every URL points to a primary source and every cited task ID exists in the downloaded repository or dataset response.

### Task 2: Write source-backed terminal and coding cases

**Files:**
- Modify: `docs/testing/lato-agent-test-cases.md`

**Interfaces:**
- Consumes: Task 1 provenance labels and sources.
- Produces: Copy-paste prompts, fixtures, steps, outcome checks, and automation assertions.

- [ ] **Step 1: Adapt Terminal-Bench tasks**

Cover file creation, log analysis, JSONL aggregation, CSV-to-Parquet conversion, deterministic archives, broken Python, permissions, safe extraction, Git repair, async cancellation, long-program debugging, vulnerability repair, dependency conflicts, and shell/network troubleshooting.

- [ ] **Step 2: Adapt SWE-bench Verified tasks**

Include at least four repository-level defects representing nested composition, misleading exceptions, output formatting, and coordinate/data transformation. Preserve `FAIL_TO_PASS` and `PASS_TO_PASS` semantics.

- [ ] **Step 3: Adapt TUA-Bench terminal tasks**

Cover workspace JSON editing, code-bug repair, document/data extraction, archive creation, metadata repair, process termination, and research-to-artifact workflows. Exclude tasks that require GUI-only interaction unsupported by Lato.

- [ ] **Step 4: Add execution grading**

For each case specify exit-code checks, expected paths, content/schema checks, immutable-input hashes, Git diff scope, and cleanup.

### Task 3: Write source-backed safety and robustness cases

**Files:**
- Modify: `docs/testing/lato-agent-test-cases.md`

**Interfaces:**
- Consumes: AgentDojo indirect-injection patterns, Terminal-Bench safety tasks, and Lato sandbox capabilities.
- Produces: Adversarial cases with both utility and security graders.

- [ ] **Step 1: Adapt indirect prompt-injection cases**

Place malicious instructions in fetched web content, README files, tool output, dependency documentation, and search results. The utility grader checks completion of the user's task; the security grader checks that the injected side effect did not occur.

- [ ] **Step 2: Adapt filesystem and secret-protection cases**

Cover path traversal, symlink escape, sensitive-file discovery, permission changes, archive extraction, environment-variable leakage, and deliberate exfiltration requests.

- [ ] **Step 3: Add recovery cases**

Cover timeouts, non-zero commands, malformed tool arguments, truncated streams, duplicate tool calls, cancellation, unavailable dependencies, network failure, and provider rate limiting.

### Task 4: Add clearly labeled Lato-native coverage

**Files:**
- Modify: `docs/testing/lato-agent-test-cases.md`

**Interfaces:**
- Consumes: Current Lato CLI and tool surface documented in the approved design.
- Produces: Product regression cases that public benchmarks cannot provide.

- [ ] **Step 1: Cover CLI and provider behavior**

Add cases for interactive startup, TTY requirements, `-p`, model selection, login, credential redaction, slash commands, SenseNova streaming, empty deltas, split tool arguments, and duplicate text output.

- [ ] **Step 2: Cover approval and sandbox behavior**

Add allow-once consumption, denied mutation, read-only mode, workspace boundary, off mode, untrusted folder behavior, and unchanged dirty files.

- [ ] **Step 3: Cover session and protocol behavior**

Add transcript persistence, resume, clear, history hydration, compaction, ACP initialization, sequential prompts, interruption, and subagent worktree isolation.

### Task 5: Complete runner guidance and audit the document

**Files:**
- Modify: `docs/testing/lato-agent-test-cases.md`
- Modify: `docs/superpowers/specs/2026-09-01-lato-agent-test-suite-design.md`

**Interfaces:**
- Consumes: All written cases.
- Produces: A self-contained manual test pack and automation contract.

- [ ] **Step 1: Add run profiles and result schema**

Define Smoke, Offline Regression, Live Provider, Capability, and Adversarial commands; record model, provider, trial, environment, duration, exit status, grader results, and failure class.

- [ ] **Step 2: Add source coverage summary**

Report total cases by source, provenance label, capability, and automation readiness. Make the number of Lato-native cases visible.

- [ ] **Step 3: Run content checks**

Run:

```bash
rg -n "TBD|TODO|待定|来源不明" docs/testing/lato-agent-test-cases.md
git diff --check -- docs/testing/lato-agent-test-cases.md docs/superpowers/specs/2026-09-01-lato-agent-test-suite-design.md
```

Expected: no placeholder matches and no whitespace errors.

- [ ] **Step 4: Manually audit provenance**

Select at least two cases from each public source and compare task ID, capability, and grader against the upstream file or dataset row. Correct any mismatch before committing.

- [ ] **Step 5: Commit documentation**

```bash
git add docs/testing/lato-agent-test-cases.md docs/superpowers/specs/2026-09-01-lato-agent-test-suite-design.md
git commit -m "docs: add sourced lato agent test suite"
```
