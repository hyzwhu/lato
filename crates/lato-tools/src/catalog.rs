use lato_core::{DescriptorError, Tool, ToolDescriptor, ToolLayer, ToolName};
use std::{collections::BTreeMap, sync::Arc};

struct RegisteredTool {
    descriptor: ToolDescriptor,
    tool: Arc<dyn Tool>,
}

#[derive(Default)]
pub struct ToolCatalog {
    tools: BTreeMap<ToolName, RegisteredTool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationOutcome {
    Inserted,
    Replaced,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid descriptor for {name}: {source}")]
    InvalidDescriptor {
        name: ToolName,
        source: DescriptorError,
    },
    #[error("tool {name} already exists and replacement was not declared")]
    ReplacementRequired { name: ToolName },
    #[error("tool {name} cannot be replaced from an equal or lower layer")]
    LowerOrEqualLayer {
        name: ToolName,
        existing: ToolLayer,
        incoming: ToolLayer,
    },
    #[error("replacement target {target} does not match tool {name}")]
    ReplacementTargetMismatch { name: ToolName, target: ToolName },
    #[error("replacement for {name} is not compatible with major version {existing_major}")]
    IncompatibleMajorVersion {
        name: ToolName,
        existing_major: u64,
        incoming_major: u64,
        declared_major: u64,
    },
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError> {
        let descriptor = tool.descriptor();
        descriptor
            .validate()
            .map_err(|source| CatalogError::InvalidDescriptor {
                name: descriptor.name.clone(),
                source,
            })?;
        let name = descriptor.name.clone();
        let Some(existing) = self.tools.get(&name) else {
            self.tools.insert(name, RegisteredTool { descriptor, tool });
            return Ok(RegistrationOutcome::Inserted);
        };
        if descriptor.source.layer <= existing.descriptor.source.layer {
            return Err(CatalogError::LowerOrEqualLayer {
                name,
                existing: existing.descriptor.source.layer,
                incoming: descriptor.source.layer,
            });
        }
        let Some(replacement) = &descriptor.source.replacement else {
            return Err(CatalogError::ReplacementRequired { name });
        };
        if replacement.target != name {
            return Err(CatalogError::ReplacementTargetMismatch {
                name,
                target: replacement.target.clone(),
            });
        }
        let existing_major = existing.descriptor.version.major;
        let incoming_major = descriptor.version.major;
        if replacement.compatible_major != existing_major || incoming_major != existing_major {
            return Err(CatalogError::IncompatibleMajorVersion {
                name,
                existing_major,
                incoming_major,
                declared_major: replacement.compatible_major,
            });
        }
        self.tools.insert(name, RegisteredTool { descriptor, tool });
        Ok(RegistrationOutcome::Replaced)
    }

    pub fn resolve(&self, name: &ToolName) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).map(|entry| entry.tool.clone())
    }

    pub fn descriptor(&self, name: &ToolName) -> Option<&ToolDescriptor> {
        self.tools.get(name).map(|entry| &entry.descriptor)
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }
}
