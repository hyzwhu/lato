use lato_core::{DescriptorError, ToolCapability, ToolDescriptor};
use std::collections::HashSet;

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CapabilityError {
    #[error("policy cannot add undeclared capability {0:?}")]
    Expansion(ToolCapability),
}

pub fn validate_capability_subset(
    declared: &[ToolCapability],
    effective: &[ToolCapability],
) -> Result<(), CapabilityError> {
    let declared = declared.iter().collect::<HashSet<_>>();
    for capability in effective {
        if !declared.contains(capability) {
            return Err(CapabilityError::Expansion(capability.clone()));
        }
    }
    Ok(())
}

pub fn validate_descriptor(descriptor: &ToolDescriptor) -> Result<(), DescriptorError> {
    descriptor.validate()
}
