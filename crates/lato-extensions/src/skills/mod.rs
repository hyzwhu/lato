pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 4 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_BODY_PEEK_BYTES: usize = 2 * 1024;
pub const MAX_SKILL_WALK_DEPTH: usize = 5;
/// Full-snapshot cap: Phase 6A itself admits at most 1,024 plugins, so this
/// still permits one candidate per plugin before later catalog truncation.
pub const MAX_SKILL_CANDIDATES: usize = 1024;
/// Full-snapshot traversal caps prevent a trusted but malformed plugin tree
/// from consuming unbounded filesystem work before candidate materialization.
pub const MAX_SKILL_DIRECTORIES_VISITED: usize = 2048;
pub const MAX_SKILL_DIRECTORY_ENTRIES: usize = 8192;
pub const MAX_MODEL_SKILL_LISTING_ENTRIES: usize = 128;
pub const MAX_MODEL_SKILL_LISTING_BYTES: usize = 64 * 1024;
pub const MAX_EXPANDED_SKILL_BODY_BYTES: usize = 128 * 1024;

pub mod catalog;
pub mod discovery;
pub mod types;

pub use catalog::SkillCatalog;
pub use discovery::discover_skills;
pub use types::{
    DiscoveredSkill, SkillDiagnostic, SkillDiscovery, SkillInvocation, SkillInvokeError,
};
