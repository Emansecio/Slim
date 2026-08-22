use serde_json::{json, Value};

use super::{
    HttpRequest, ProviderAdapter, ProviderAuth, ProviderConfig, ProviderContentBlock,
    ProviderError, ProviderEvent, ProviderKind, ProviderMessage,
};

pub struct OpenAiCodexAdapter {
    config: ProviderConfig,
}

impl OpenAiCodexAdapter {
    pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
        if config.kind != ProviderKind::OpenAiCodex {
            return Err(ProviderError::InvalidResponse {
                message: "provider kind mismatch".into(),
            });
        }
        match config.auth() {
            ProviderAuth::OAuth {
                account_id: Some(account_id),
                ..
            } if !account_id.is_empty() => Ok(Self { config }),
            _ => Err(ProviderError::InvalidResponse {
                message: "Codex OAuth requires a ChatGPT account id".into(),
            }),
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let ProviderAuth::OAuth {
            access_token,
            account_id: Some(account_id),
        } = self.config.auth()
        else {
            unreachable!("validated by constructor")
        };
        vec![
            ("Authorization".into(), format!("Bearer {access_token}")),
            ("chatgpt-account-id".into(), account_id.clone()),
            ("originator".into(), "slim".into()),
            ("User-Agent".into(), "slim/0.1.0".into()),
            ("OpenAI-Beta".into(), "responses=experimental".into()),
            ("accept".into(), "text/event-stream".into()),
            ("content-type".into(), "application/json".into()),
        ]
    }

    fn request(&self, messages: &[ProviderMessage], tools: &[Value]) -> HttpRequest {
        let mut input = Vec::new();
        for message in messages {
            if message.role == "tool" {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.content,
                }));
                continue;
            }
            input.push(json!({
                "role": message.role,
                "content": codex_content(message),
            }));
            for call in &message.tool_calls {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }));
            }
        }
        let tools = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.get("name").cloned().unwrap_or(Value::Null),
                    "description": tool.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"})),
                })
            })
            .collect::<Vec<_>>();
        let mut body = json!({
            "model": self.config.model,
            "store": false,
            "stream": true,
            "instructions": self.config.effective_system_prompt().unwrap_or(""),
            "input": input,
            "include": ["reasoning.encrypted_content"],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "tools": tools,
        });
        if let Some(effort) = self.config.reasoning_effort.as_deref() {
            body["reasoning"] = json!({ "effort": effort });
        }
        HttpRequest {
            url: codex_url(&self.config.endpoint),
            headers: self.headers(),
            body: body.to_string(),
        }
    }
}

fn codex_content(message: &ProviderMessage) -> Vec<Value> {
    let text_type = if message.role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let mut content = Vec::new();
    if !message.content.is_empty() {
        content.push(json!({"type": text_type, "text": message.content}));
    }
    for block in &message.content_blocks {
        match block {
            ProviderContentBlock::Text(text) => {
                content.push(json!({"type": text_type, "text": text}));
            }
            ProviderContentBlock::Image { media_type, data } if message.role != "assistant" => {
                content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                }));
            }
            ProviderContentBlock::Image { .. } => content.push(json!({
                "type": text_type,
                "text": "[assistant image omitted]",
            })),
            ProviderContentBlock::Audio { media_type, data } => content.push(json!({
                "type": text_type,
                "text": format!("[audio omitted: {media_type}; base64-bytes={}]", data.len()),
            })),
            ProviderContentBlock::File { media_type, data } => content.push(json!({
                "type": text_type,
                "text": format!("[file omitted: {media_type}; base64-bytes={}]", data.len()),
            })),
            ProviderContentBlock::Unsupported { kind } => content.push(json!({
                "type": text_type,
                "text": format!("[unsupported content omitted: {kind}]"),
            })),
        }
    }
    if content.is_empty() {
        content.push(json!({"type": text_type, "text": ""}));
    }
    content
}

impl ProviderAdapter for OpenAiCodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCodex
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    fn build_request(&self, prompt: &str) -> HttpRequest {
        self.request(&[ProviderMessage::user(prompt)], &[])
    }

    fn build_messages_request(&self, messages: &[ProviderMessage]) -> HttpRequest {
        self.request(messages, &[])
    }

    fn build_messages_request_with_tools(
        &self,
        messages: &[ProviderMessage],
        tools: &[Value],
    ) -> HttpRequest {
        self.request(messages, tools)
    }

    fn parse_event(&self, value: &Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let events = match event_type {
            "response.output_text.delta" => value
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![ProviderEvent::TextDelta(text.into())])
                .unwrap_or_default(),
            "response.reasoning_summary_text.delta" => value
                .get("delta")
                .and_then(Value::as_str)
                .map(|text| vec![ProviderEvent::ReasoningDelta(text.into())])
                .unwrap_or_default(),
            "response.output_item.added" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    vec![ProviderEvent::ToolCallDelta {
                        index: value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .map(|index| index as u32),
                        id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        name: item.get("name").and_then(Value::as_str).map(str::to_owned),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    }]
                } else {
                    Vec::new()
                }
            }
            "response.function_call_arguments.delta" => vec![ProviderEvent::ToolCallDelta {
                index: value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .map(|index| index as u32),
                id: None,
                name: None,
                arguments: value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }],
            "response.output_item.done" => {
                let item = value.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    match (
                        item.get("name").and_then(Value::as_str),
                        item.get("arguments").and_then(Value::as_str),
                    ) {
                        (Some(name), Some(arguments)) => vec![ProviderEvent::ToolCall {
                            name: name.into(),
                            arguments: arguments.into(),
                        }],
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                }
            }
            "response.completed" => {
                let usage = value
                    .get("response")
                    .and_then(|response| response.get("usage"));
                let mut events = Vec::new();
                if let Some(usage) = usage {
                    events.push(ProviderEvent::Usage {
                        input_tokens: usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        output_tokens: usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                    });
                }
                events.push(ProviderEvent::Stopped {
                    reason: "completed".into(),
                });
                events
            }
            "error" | "response.failed" => {
                return Err(ProviderError::Remote {
                    message: value
                        .pointer("/error/message")
                        .or_else(|| value.pointer("/response/error/message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Codex provider error")
                        .chars()
                        .take(512)
                        .collect(),
                })
            }
            _ => Vec::new(),
        };
        Ok(events)
    }
}

fn codex_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.ends_with("/codex/responses") {
        endpoint.into()
    } else if endpoint.ends_with("/codex") {
        format!("{endpoint}/responses")
    } else {
        format!("{endpoint}/codex/responses")
    }
}
