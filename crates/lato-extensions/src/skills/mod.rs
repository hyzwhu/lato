pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 4 * 1024;
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
pub const MAX_BODY_PEEK_BYTES: usize = 2 * 1024;
pub const MAX_SKILL_WALK_DEPTH: usize = 5;

pub mod discovery;
pub mod types;

pub use discovery::discover_skills;
pub use types::{DiscoveredSkill, SkillDiagnostic, SkillDiscovery};
