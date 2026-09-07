pub mod discovery;
pub mod manifest;
pub mod registry;
pub mod trust;

pub use discovery::{
    DiscoveredPlugin, DiscoveryConfig, DiscoveryDiagnostic, DiscoveryResult, PluginId,
    PluginOrigin, PluginScope, discover_plugins,
};
pub use manifest::{
    Author, ManifestError, ManifestLoadResult, PathOrInline, PathOrPaths, PluginManifest,
    load_manifest, name_from_dirname,
};
pub use registry::{
    CapabilityCeiling, LoadedPlugin, PluginComponentKind, PluginConfig, PluginSnapshot,
    RegistryBuildError, build_snapshot,
};

pub const MAX_PLUGIN_NAME_LEN: usize = 64;
pub const MAX_COMPONENT_PATHS: usize = 64;
pub const MAX_COMPONENT_PATH_BYTES: usize = 4 * 1024;
pub const MAX_DISCOVERED_PLUGINS: usize = 1024;
pub const MAX_DISCOVERY_DIAGNOSTICS: usize = 128;
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 512;
