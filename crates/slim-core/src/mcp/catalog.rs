use crate::{mcp::canonical_name, OperatingMode};

#[derive(Clone, Debug)]
pub struct McpCatalog {
    server: String,
    tools: Vec<String>,
    resources: Vec<String>,
    prompts: Vec<String>,
    selected_resources: Vec<String>,
    selected_prompts: Vec<String>,
}

impl McpCatalog {
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            selected_resources: Vec::new(),
            selected_prompts: Vec::new(),
        }
    }

    pub fn add_tool(&mut self, tool: impl Into<String>) {
        self.tools.push(tool.into());
    }

    pub fn add_resource(&mut self, resource: impl Into<String>) {
        self.resources.push(resource.into());
    }

    pub fn add_prompt(&mut self, prompt: impl Into<String>) {
        self.prompts.push(prompt.into());
    }

    pub fn tools_for_mode(&self, mode: OperatingMode) -> Vec<String> {
        if mode != OperatingMode::Auto {
            return Vec::new();
        }
        self.tools
            .iter()
            .map(|tool| canonical_name(&self.server, tool))
            .collect()
    }

    pub fn select_resource(&mut self, resource: &str) -> Result<(), String> {
        if self.resources.iter().any(|candidate| candidate == resource) {
            if !self
                .selected_resources
                .iter()
                .any(|candidate| candidate == resource)
            {
                self.selected_resources.push(resource.into());
            }
            Ok(())
        } else {
            Err(format!("unknown MCP resource: {resource}"))
        }
    }

    pub fn selected_resources(&self) -> &[String] {
        &self.selected_resources
    }

    pub fn select_prompt(&mut self, prompt: &str) -> Result<(), String> {
        if self.prompts.iter().any(|candidate| candidate == prompt) {
            if !self
                .selected_prompts
                .iter()
                .any(|candidate| candidate == prompt)
            {
                self.selected_prompts.push(prompt.into());
            }
            Ok(())
        } else {
            Err(format!("unknown MCP prompt: {prompt}"))
        }
    }

    pub fn selected_prompts(&self) -> &[String] {
        &self.selected_prompts
    }

    pub fn call_tool(
        &self,
        mode: OperatingMode,
        tool: &str,
        arguments: &str,
    ) -> Result<String, String> {
        if mode != OperatingMode::Auto {
            return Err("MCP tools are unavailable outside Auto".into());
        }
        if !self.tools.iter().any(|candidate| candidate == tool) {
            return Err(format!("unknown MCP tool: {tool}"));
        }
        Ok(format!(
            "{}({arguments})",
            canonical_name(&self.server, tool)
        ))
    }
}
