use super::*;

pub(super) fn skill_tool_definition() -> Value {
    json!({
        "name": "skill",
        "description": "List skills with {list:true}; load SKILL.md by name. Scripts require explicit host trust and cannot run from model tool calls.",
        "input_schema": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "list": {"type": "boolean"},
                "script": {"type": "string", "description": "Filename inside skill directory; no ./ or absolute path. Default run.ps1."}
            },
            "additionalProperties": false
        }
    })
}

pub(super) fn render_skill_list(discovery: &DiscoveryResult) -> ToolResult {
    let mut lines: Vec<String> = discovery
        .active_entries()
        .iter()
        .map(|entry| format!("{}: {}", entry.name, entry.metadata.description))
        .collect();
    if lines.is_empty() {
        lines.push("no skills found".into());
    }
    lines.extend(discovery.diagnostic_lines());
    ToolResult::ok("skill", lines.join("\n"))
}

pub(super) fn run_skill_dispatch(
    mode: crate::OperatingMode,
    cwd: &Path,
    arguments: &str,
    cached: Option<DiscoveryResult>,
) -> ToolResult {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult::fail("skill", format!("invalid skill arguments: {error}"))
        }
    };
    let list = match args.get("list") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return ToolResult::fail("skill", "invalid skill arguments: list must be a boolean")
        }
    };
    let script = match args.get("script") {
        None => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => {
            return ToolResult::fail("skill", "invalid skill arguments: script must be a string")
        }
    };
    if list {
        return match cached {
            Some(discovery) => render_skill_list(&discovery),
            None => match discover_workspace(cwd) {
                Ok(discovery) => render_skill_list(&discovery),
                Err(error) => ToolResult::fail("skill", format!("skill discovery failed: {error}")),
            },
        };
    }
    let Some(name) = args
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return ToolResult::fail("skill", "skill needs a name (or list:true)");
    };
    let discovery = match cached {
        Some(discovery) => discovery,
        None => {
            let Ok(discovery) = discover_workspace(cwd) else {
                return ToolResult::fail("skill", format!("skill discovery failed for {name}"));
            };
            discovery
        }
    };
    let Some(entry) = discovery.active(name) else {
        return ToolResult::fail("skill", format!("skill not found: {name}"));
    };
    if script.is_none() {
        return match crate::skills::read_body(entry.path.join("SKILL.md")) {
            Ok(body) => ToolResult::ok("skill", body),
            Err(error) => ToolResult::fail("skill", format!("skill instructions failed: {error}")),
        };
    }
    let script = crate::skills::default_skill_script(script);
    if let Some(body) = crate::skills::fallback_skill_body(&entry.path, script) {
        return ToolResult::ok("skill", body);
    }
    // The model cannot grant user trust, so a script never runs from here.
    let error = crate::skills::validate_invocation(mode, false)
        .err()
        .unwrap_or(crate::skills::SkillInvocationError::TrustRequired);
    ToolResult::fail("skill", format!("skill failed: {error}"))
}

impl Runtime {
    /// Skill discovery memoized for the current loop run, keyed by cwd.
    /// Returns `None` when discovery fails so the dispatcher falls back to
    /// direct discovery, which owns the error messages.
    pub(super) fn cached_skill_discovery(&mut self, cwd: &Path) -> Option<DiscoveryResult> {
        if let Some((dir, discovery)) = self.skill_discovery_cache.as_ref() {
            if dir == cwd {
                return discovery.clone();
            }
        }
        let discovery = discover_workspace(cwd).ok();
        self.skill_discovery_cache = Some((cwd.to_path_buf(), discovery.clone()));
        discovery
    }

    pub(super) async fn execute_skill(
        &mut self,
        mode: crate::OperatingMode,
        cwd: &Path,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut seq)?;
        let cached_discovery = self.cached_skill_discovery(cwd);
        let mut result = run_skill_dispatch(mode, cwd, invocation.arguments, cached_discovery);
        if self.is_cancelled() {
            result.success = false;
        }
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }
}
