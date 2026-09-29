use super::*;

impl Runtime {
    /// ask_question routes through the interaction channel (TUI/headless
    /// respond out-of-band); the result is the structured human answer.
    pub(super) async fn execute_ask_question(
        &mut self,
        invocation: ToolInvocation<'_>,
        next_seq: u64,
    ) -> Result<(ToolResult, u64), ProviderError> {
        self.ensure_not_cancelled()?;
        let mut seq = next_seq;
        let started_at = self.begin_tool(invocation, &mut seq)?;

        let route = self.interaction_route.clone();
        let cancellation = self.cancellation.clone();
        let mut result = match route {
            None => ToolResult::fail(
                invocation.name,
                "ask_question unavailable: no interaction route configured",
            ),
            Some(route) => match AskQuestion::parse(invocation.arguments) {
                Err(error) => ToolResult::fail(
                    invocation.name,
                    format!("invalid ask_question arguments: {error}"),
                ),
                Ok(question) => {
                    // The request id is the provider tool-call id. The TUI
                    // namespaces it by run only at the projection boundary.
                    match InteractionRequestId::new(invocation.call_id.to_owned()) {
                        Err(_) => {
                            ToolResult::fail(invocation.name, "invalid interaction request id")
                        }
                        Ok(request_id) => match route.register(request_id.clone()) {
                            Err(error) => {
                                ToolResult::fail(invocation.name, format!("ask_question: {error}"))
                            }
                            Ok(pending) => {
                                push_runtime_event(
                                    &mut self.app,
                                    &mut seq,
                                    crate::EventKind::QuestionRequired {
                                        request_id: request_id.as_str().to_owned(),
                                        question: question.question,
                                        options: question.options,
                                        persisted: false,
                                    },
                                )?;

                                let received = if let Some(cancellation) = cancellation {
                                    tokio::select! {
                                        answer = pending.receive() => Some(answer),
                                        _ = cancellation.cancelled() => None,
                                    }
                                } else {
                                    Some(pending.receive().await)
                                };

                                match received {
                                    Some(Ok(answer)) => {
                                        let output =
                                            serde_json::to_string(&answer).map_err(|error| {
                                                ProviderError::InvalidResponse {
                                                    message: format!(
                                                    "failed to serialize question answer: {error}"
                                                ),
                                                }
                                            })?;
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: true,
                                                message: "answer accepted".into(),
                                            },
                                        )?;
                                        ToolResult::ok(invocation.name, output)
                                    }
                                    Some(Err(error)) => {
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: false,
                                                message: error.to_string(),
                                            },
                                        )?;
                                        ToolResult::fail(
                                            invocation.name,
                                            format!("interaction declined: {error}"),
                                        )
                                    }
                                    None => {
                                        push_runtime_event(
                                            &mut self.app,
                                            &mut seq,
                                            crate::EventKind::InteractionAcknowledged {
                                                request_id: request_id.as_str().to_owned(),
                                                accepted: false,
                                                message: "interaction cancelled".into(),
                                            },
                                        )?;
                                        ToolResult::fail(invocation.name, "ask_question cancelled")
                                    }
                                }
                            }
                        },
                    }
                }
            },
        };
        if self.is_cancelled() {
            result.success = false;
        }
        self.finish_tool(invocation, &mut result, started_at, &mut seq, None)?;
        Ok((result, seq))
    }
}
