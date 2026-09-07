pub mod manifest;

pub use manifest::{
    Author, ManifestError, ManifestLoadResult, PathOrInline, PathOrPaths, PluginManifest,
    load_manifest, name_from_dirname,
};

pub const MAX_PLUGIN_NAME_LEN: usize = 64;
pub const MAX_COMPONENT_PATHS: usize = 64;
pub const MAX_COMPONENT_PATH_BYTES: usize = 4 * 1024;
