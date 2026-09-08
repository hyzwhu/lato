// Derived from: Grok Build@bb7f39d5858cbf5e00de639367f59debbdcb0138 hook runtime
// License: Apache-2.0
// Lato changes: immutable registries, bounded runners, and authority-preserving typed outcomes

mod command;
mod config;
mod event;
mod result;

pub use config::*;
pub use event::*;
pub use result::*;

pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
pub const MAX_RUNNER_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_REASON_CHARS: usize = 256;
pub const MAX_FEEDBACK_CHARS: usize = 10_000;
pub const MAX_REPLACEMENT_CHARS: usize = 64 * 1024;
pub const MAX_STOP_CONTINUATIONS: usize = 8;
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;
pub use command::*;
