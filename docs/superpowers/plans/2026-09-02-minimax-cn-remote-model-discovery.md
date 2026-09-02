# MiniMax CN Remote Model Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `minimax-cn` model selection retry the authoritative remote catalog and stop with a clear error instead of using built-in or cached models when the refresh fails.

**Architecture:** Extend the remote catalog client with an explicit refresh policy so strict callers can bypass fresh cache reads, reject cache-based HTTP outcomes, and retry transient response/decoding failures. Keep the provider-agnostic default behavior intact, while the CLI applies strict behavior only to `minimax-cn` and presents only models from the successful remote response.

**Tech Stack:** Rust, Tokio, Reqwest, Serde JSON, Cargo test harness

## Global Constraints

- The model choices for `minimax-cn` must come exclusively from the current remote catalog response.
- All retry attempts failing must stop interactive model selection.
- Neither built-in models nor previously cached remote models may satisfy a failed `minimax-cn` refresh.
- Diagnostics must not expose API keys or authorization headers.
- Other providers retain their current discovery behavior.

---

### Task 1: Strict, retrying remote catalog refresh

**Files:**
- Modify: `crates/lato-ai/src/provider.rs:147-256`
- Test: `crates/lato-ai/src/provider.rs:309-344`

**Interfaces:**
- Consumes: `ProviderSpec`, `ProviderModelsStore`, `CustomModel`, and the existing remote catalog JSON formats.
- Produces: `pub enum RemoteCatalogRefreshPolicy { Cached, Authoritative { attempts: usize } }` and `pub async fn refresh_remote_provider_catalog_with_policy(...) -> Result<Vec<CustomModel>, String>`; the existing `refresh_remote_provider_catalog(...)` remains a cached-policy wrapper.

- [ ] **Step 1: Write failing tests for authoritative retry and terminal failure**

Add Tokio tests whose local TCP servers return an empty `200` response followed by valid JSON, and repeated HTML/invalid JSON responses. Call the new policy-aware function with `RemoteCatalogRefreshPolicy::Authoritative { attempts: 2 }`. Assert that the first case returns the remote model and receives two requests; assert that the second returns an error containing `minimax-cn`, `invalid JSON`, and `after 2 attempts`, without containing a sentinel API key.

- [ ] **Step 2: Run the focused tests and verify the new API is missing**

Run: `cargo test -p lato-ai provider::tests::authoritative_remote_catalog -- --nocapture`

Expected: compilation fails because `RemoteCatalogRefreshPolicy` and `refresh_remote_provider_catalog_with_policy` do not exist.

- [ ] **Step 3: Implement the policy-aware refresh loop**

Add the public policy enum and make the existing function delegate to the new function with `Cached`. In authoritative mode:

```rust
let attempts = match policy {
    RemoteCatalogRefreshPolicy::Cached => 1,
    RemoteCatalogRefreshPolicy::Authoritative { attempts } => attempts.max(1),
};
```

Skip the freshness-window return, do not send `If-None-Match`, and reject `304`, `404`, and `501` instead of returning stored models. Build a new request for every attempt. Read successful responses with `response.bytes().await`, reject empty/whitespace-only bytes, then decode with `serde_json::from_slice`. Retry transport, empty-body, and JSON-decoding errors until the attempt budget is exhausted. Return a provider-scoped message ending in `after {attempts} attempts`; sanitize malformed response previews by replacing control characters, limiting output to 160 characters, and never incorporating request headers or credential state. Only write `ProviderModelsStore` after a successfully parsed catalog.

- [ ] **Step 4: Run provider tests**

Run: `cargo test -p lato-ai provider::tests -- --nocapture`

Expected: all provider tests pass, including the existing cache freshness test and the new authoritative tests.

- [ ] **Step 5: Commit the remote refresh implementation**

```bash
git add crates/lato-ai/src/provider.rs
git commit -m "fix: retry authoritative remote model catalogs"
```

### Task 2: Enforce strict MiniMax CN selection semantics

**Files:**
- Modify: `src/cli.rs:450-519`
- Modify: `src/cli.rs:617-643`
- Test: `src/cli.rs:934-952`

**Interfaces:**
- Consumes: `refresh_remote_provider_catalog_with_policy` and `RemoteCatalogRefreshPolicy::Authoritative { attempts: 3 }` from Task 1.
- Produces: `fn requires_authoritative_remote_models(provider: &str) -> bool`, returning true only for `minimax-cn`, and CLI behavior that returns remote-only choices or an error.

- [ ] **Step 1: Write failing policy and model-resolution tests**

Add a unit test asserting:

```rust
assert!(requires_authoritative_remote_models("minimax-cn"));
assert!(!requires_authoritative_remote_models("minimax"));
assert!(!requires_authoritative_remote_models("sensenova"));
```

Extract the remote/fallback merge decision into a small pure helper. Test that strict `minimax-cn` returns only a remote `MiniMax-M3` entry and rejects both an empty remote list and a discovery error instead of returning the built-in `MiniMax-M2.1` fallback.

- [ ] **Step 2: Run the focused CLI tests and verify failure**

Run: `cargo test --bin lato cli::tests::minimax_cn -- --nocapture`

Expected: compilation fails because the strict-policy helper and selection helper do not exist.

- [ ] **Step 3: Wire strict discovery into interactive configuration**

In `discover_provider_models`, choose the policy explicitly:

```rust
let policy = if requires_authoritative_remote_models(provider) {
    RemoteCatalogRefreshPolicy::Authoritative { attempts: 3 }
} else {
    RemoteCatalogRefreshPolicy::Cached
};
```

In `configure_interactively`, route the discovery result through the pure selection helper. For strict providers, return the successful non-empty remote list without merging `fallback`; convert empty lists and errors into `Err("could not refresh authoritative model catalog for minimax-cn: ...; model selection stopped")`. Preserve the current merge/fallback path for all other providers. Print `Choose a model:` only after this resolution succeeds.

- [ ] **Step 4: Run focused and full tests**

Run: `cargo test -p lato-ai provider::tests -- --nocapture`

Expected: all provider catalog tests pass.

Run: `cargo test --bin lato cli::tests -- --nocapture`

Expected: all CLI unit tests pass.

Run: `cargo test --workspace`

Expected: the full workspace test suite passes.

- [ ] **Step 5: Commit the CLI enforcement**

```bash
git add src/cli.rs
git commit -m "fix: require remote MiniMax CN model choices"
```

### Task 3: Package and smoke-test the repaired CLI

**Files:**
- No source changes expected.

**Interfaces:**
- Consumes: completed Tasks 1 and 2.
- Produces: an updated locally installed `lato` executable.

- [ ] **Step 1: Install from the repository root**

Run: `cargo install --path .`

Expected: Cargo reports that the `lato` executable was installed or replaced successfully.

- [ ] **Step 2: Verify the installed executable**

Run: `lato --version`

Expected: the command exits successfully and prints the installed Lato version.

- [ ] **Step 3: Check the final working tree**

Run: `git status --short`

Expected: only pre-existing user-owned files such as untracked `AGENTS.md` remain; no implementation files are uncommitted.
