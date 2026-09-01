use crate::{
    BuiltinAdapterError, BuiltinToolEnvironment, CatalogError, RegistrationOutcome, ToolCatalog,
    builtin_tools,
};
use lato_core::{Retryability, Tool, ToolContext, ToolDescriptor, ToolError, ToolName, ToolOutput};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub struct ToolRuntime {
    catalog: ToolCatalog,
    wire_names: BTreeMap<String, ToolName>,
}

#[derive(Default)]
pub struct ToolRuntimeBuilder {
    catalog: ToolCatalog,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Builtin(#[from] BuiltinAdapterError),
    #[error("model-facing tool name {wire_name} is ambiguous between {first} and {second}")]
    AmbiguousWireName {
        wire_name: String,
        first: ToolName,
        second: ToolName,
    },
}

impl ToolRuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<RegistrationOutcome, CatalogError> {
        self.catalog.register(tool)
    }

    pub fn register_builtin_tools(
        &mut self,
        environment: BuiltinToolEnvironment,
    ) -> Result<(), RuntimeBuildError> {
        for tool in builtin_tools(environment)? {
            self.register(tool)?;
        }
        Ok(())
    }

    pub fn build(self) -> Result<ToolRuntime, RuntimeBuildError> {
        let mut wire_names = BTreeMap::new();
        for descriptor in self.catalog.descriptors() {
            let wire_name = descriptor.name.local_name().to_owned();
            if let Some(first) = wire_names.insert(wire_name.clone(), descriptor.name.clone()) {
                return Err(RuntimeBuildError::AmbiguousWireName {
                    wire_name,
                    first,
                    second: descriptor.name,
                });
            }
        }
        Ok(ToolRuntime {
            catalog: self.catalog,
            wire_names,
        })
    }
}

impl ToolRuntime {
    pub fn model_definitions(&self) -> Vec<Value> {
        self.catalog
            .descriptors()
            .into_iter()
            .map(|descriptor| {
                json!({
                    "type": "function",
                    "function": {
                        "name": descriptor.name.local_name(),
                        "description": descriptor.description,
                        "parameters": descriptor.input_schema,
                    }
                })
            })
            .collect()
    }

    pub async fn invoke(
        &self,
        context: ToolContext,
        wire_name: &str,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if context.cancellation.is_cancelled() {
            return Err(ToolError::new(
                "tool.cancelled",
                "tool call was cancelled",
                Retryability::Never,
            ));
        }

        let Some(canonical) = self.resolve_wire_name(wire_name) else {
            return Err(not_found(wire_name));
        };
        let Some(tool) = self.catalog.resolve(&canonical) else {
            return Err(not_found(wire_name));
        };
        tool.invoke(context, arguments).await
    }

    pub fn descriptor_for_wire_name(&self, wire_name: &str) -> Option<ToolDescriptor> {
        let canonical = self.resolve_wire_name(wire_name)?;
        self.catalog.descriptor(&canonical).cloned()
    }

    fn resolve_wire_name(&self, wire_name: &str) -> Option<ToolName> {
        let normalized = wire_name.strip_prefix("Lato:").unwrap_or(wire_name);
        let normalized = if normalized == "write" {
            "write_file"
        } else {
            normalized
        };
        if normalized.contains(':') {
            ToolName::parse(normalized).ok()
        } else {
            self.wire_names.get(normalized).cloned()
        }
    }
}

pub fn builtin_tool_runtime(
    environment: BuiltinToolEnvironment,
) -> Result<Arc<ToolRuntime>, RuntimeBuildError> {
    let mut builder = ToolRuntimeBuilder::new();
    builder.register_builtin_tools(environment)?;
    Ok(Arc::new(builder.build()?))
}

fn not_found(wire_name: &str) -> ToolError {
    ToolError::new(
        "tool.not_found",
        format!("tool {wire_name} was not found"),
        Retryability::Never,
    )
}
