//! Rhai script host channel, journal, meta, and run outcome (Phase 7B3).

pub mod host;
pub mod journal;
pub mod meta;
pub mod run;

pub const MAX_PARALLEL: usize = 1_024;
pub const MAX_HOST_CALLS: u64 = 10_000;

pub const MAX_WORKFLOW_DESCRIPTION_LEN: usize = 1_024;
pub const MAX_WORKFLOW_WHEN_TO_USE_LEN: usize = 2_048;
pub const MAX_WORKFLOW_PHASES: usize = 64;
pub const MAX_PHASE_TITLE_LEN: usize = 128;
pub const MAX_PHASE_DETAIL_LEN: usize = 1_024;

pub use host::{AgentOpts, AgentResult, BudgetState, HostError, WorkflowHostRequest};
pub use journal::Journal;
pub use meta::extract_meta;
pub use run::ScriptOutcome;

pub(crate) fn with_rhai_hint(msg: String) -> String {
    let hint = if msg.contains("Expression exceeds maximum complexity") {
        "a single expression nests too deep — usually one long chained `+` string \
         concatenation. Split it into multiple `+=` statements."
    } else if msg.contains("reserved keyword") {
        "Rhai reserves identifiers it doesn't use — `shared`, `sync`, `async`, `await`, \
         `spawn`, `go`, `thread`, `new`, `match`, `case`, `default`, `void`, `null`, \
         `nil`, `exit`, `static`, `var` — rename the variable (`shared` → `has_shared`)."
    } else if msg.contains("getter is not registered for type 'char'") {
        "indexing a string yields a `char`, so field access on it fails — you likely \
         indexed a string you expected to be an array (e.g. unparsed JSON in an agent \
         output). Check with `type_of(x)`; slice strings with `s.sub_string(start, len)`."
    } else {
        return msg;
    };
    format!("{msg}\nhint: {hint}")
}
