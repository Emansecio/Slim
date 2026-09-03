//! Typed, bounded model-to-user interactions.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

pub const MAX_QUESTION_CHARS: usize = 1_024;
pub const MAX_QUESTION_OPTIONS: usize = 5;
pub const MAX_OPTION_LABEL_CHARS: usize = 80;
pub const MAX_OPTION_DESCRIPTION_CHARS: usize = 256;
pub const MAX_CUSTOM_ANSWER_BYTES: usize = 16 * 1024;
const MAX_INTERACTIONS_PER_ROUTE: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionError {
    InvalidJson { message: String },
    InvalidQuestion,
    InvalidOptionCount { count: usize },
    InvalidOption { index: usize, field: &'static str },
    DuplicateOption { index: usize },
    InvalidAnswer,
    InvalidRequestId,
    DuplicateRequest { request_id: String },
    StaleRequest { request_id: String },
    RouteCapacity,
    RouteClosed,
}

impl fmt::Display for InteractionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson { message } => {
                write!(formatter, "invalid ask_question payload: {message}")
            }
            Self::InvalidQuestion => {
                formatter.write_str("question must be one non-empty bounded line")
            }
            Self::InvalidOptionCount { count } => {
                write!(
                    formatter,
                    "question requires zero or 2..=5 options, got {count}"
                )
            }
            Self::InvalidOption { index, field } => {
                write!(formatter, "invalid question option {index} field {field}")
            }
            Self::DuplicateOption { index } => {
                write!(formatter, "duplicate question option {index}")
            }
            Self::InvalidAnswer => {
                formatter.write_str("question answer must be non-empty and at most 16 KiB")
            }
            Self::InvalidRequestId => formatter.write_str("interaction request id is invalid"),
            Self::DuplicateRequest { request_id } => {
                write!(
                    formatter,
                    "interaction request already registered: {request_id}"
                )
            }
            Self::StaleRequest { request_id } => {
                write!(
                    formatter,
                    "interaction request is not pending: {request_id}"
                )
            }
            Self::RouteCapacity => formatter.write_str("interaction route capacity exhausted"),
            Self::RouteClosed => formatter.write_str("interaction route is closed"),
        }
    }
}

impl std::error::Error for InteractionError {}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InteractionRequestId(String);

impl InteractionRequestId {
    pub fn new(value: impl Into<String>) -> Result<Self, InteractionError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.trim() == value
            && value.chars().count() <= 256
            && !value.chars().any(char::is_control);
        valid
            .then_some(Self(value))
            .ok_or(InteractionError::InvalidRequestId)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum QuestionAnswerSource {
    Option { option_index: usize },
    Custom,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QuestionAnswer {
    pub answer: String,
    #[serde(flatten)]
    pub source: QuestionAnswerSource,
}

impl QuestionAnswer {
    pub fn option(index: usize, answer: impl Into<String>) -> Result<Self, InteractionError> {
        Self::new(
            answer.into(),
            QuestionAnswerSource::Option {
                option_index: index,
            },
        )
    }

    pub fn custom(answer: impl Into<String>) -> Result<Self, InteractionError> {
        Self::new(answer.into(), QuestionAnswerSource::Custom)
    }

    fn new(answer: String, source: QuestionAnswerSource) -> Result<Self, InteractionError> {
        if answer.trim().is_empty() || answer.len() > MAX_CUSTOM_ANSWER_BYTES {
            return Err(InteractionError::InvalidAnswer);
        }
        Ok(Self { answer, source })
    }
}

type PendingSender = oneshot::Sender<QuestionAnswer>;

#[derive(Default)]
struct InteractionRouteState {
    pending: HashMap<InteractionRequestId, PendingSender>,
    completed: HashSet<InteractionRequestId>,
}

#[derive(Clone, Default)]
pub struct InteractionRoute {
    state: Arc<Mutex<InteractionRouteState>>,
}

#[derive(Clone, Default)]
pub struct InteractionResponder {
    state: Arc<Mutex<InteractionRouteState>>,
}

pub struct PendingQuestion {
    request_id: InteractionRequestId,
    receiver: Option<oneshot::Receiver<QuestionAnswer>>,
    state: Arc<Mutex<InteractionRouteState>>,
}

pub fn interaction_route() -> (InteractionRoute, InteractionResponder) {
    let state = Arc::new(Mutex::new(InteractionRouteState::default()));
    (
        InteractionRoute {
            state: Arc::clone(&state),
        },
        InteractionResponder { state },
    )
}

pub fn ask_question_definition() -> Value {
    json!({
        "name": "ask_question",
        "description": "Ask the user one bounded question when a missing choice blocks progress. Use options for concrete alternatives; omit options for a free-form answer.",
        "input_schema": {
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_QUESTION_CHARS
                },
                "options": {
                    "type": "array",
                    "maxItems": MAX_QUESTION_OPTIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": MAX_OPTION_LABEL_CHARS
                            },
                            "description": {
                                "type": "string",
                                "maxLength": MAX_OPTION_DESCRIPTION_CHARS
                            }
                        },
                        "required": ["label"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["question"],
            "additionalProperties": false
        }
    })
}

impl InteractionRoute {
    pub fn register(
        &self,
        request_id: InteractionRequestId,
    ) -> Result<PendingQuestion, InteractionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| InteractionError::RouteClosed)?;
        if state.pending.contains_key(&request_id) || state.completed.contains(&request_id) {
            return Err(InteractionError::DuplicateRequest {
                request_id: request_id.as_str().to_owned(),
            });
        }
        if state.pending.len() >= MAX_INTERACTIONS_PER_ROUTE {
            return Err(InteractionError::RouteCapacity);
        }
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(request_id.clone(), sender);
        drop(state);
        Ok(PendingQuestion {
            request_id,
            receiver: Some(receiver),
            state: Arc::clone(&self.state),
        })
    }
}

impl InteractionResponder {
    pub fn answer(
        &self,
        request_id: InteractionRequestId,
        answer: QuestionAnswer,
    ) -> Result<(), InteractionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| InteractionError::RouteClosed)?;
        let sender =
            state
                .pending
                .remove(&request_id)
                .ok_or_else(|| InteractionError::StaleRequest {
                    request_id: request_id.as_str().to_owned(),
                })?;
        state.completed.insert(request_id);
        drop(state);
        sender
            .send(answer)
            .map_err(|_| InteractionError::RouteClosed)
    }
}

impl PendingQuestion {
    pub async fn receive(mut self) -> Result<QuestionAnswer, InteractionError> {
        let receiver = self.receiver.take().ok_or(InteractionError::RouteClosed)?;
        receiver.await.map_err(|_| InteractionError::RouteClosed)
    }
}

impl Drop for PendingQuestion {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if state.pending.remove(&self.request_id).is_some() {
                state.completed.insert(self.request_id.clone());
            }
        }
    }
}

impl AskQuestion {
    pub fn parse(arguments: &str) -> Result<Self, InteractionError> {
        let question: Self =
            serde_json::from_str(arguments).map_err(|error| InteractionError::InvalidJson {
                message: error.to_string(),
            })?;
        question.validate()?;
        Ok(question)
    }

    fn validate(&self) -> Result<(), InteractionError> {
        if !valid_single_line(&self.question, 1, MAX_QUESTION_CHARS) {
            return Err(InteractionError::InvalidQuestion);
        }
        if self.options.len() == 1 || self.options.len() > MAX_QUESTION_OPTIONS {
            return Err(InteractionError::InvalidOptionCount {
                count: self.options.len(),
            });
        }
        for (index, option) in self.options.iter().enumerate() {
            if !valid_single_line(&option.label, 1, MAX_OPTION_LABEL_CHARS) {
                return Err(InteractionError::InvalidOption {
                    index,
                    field: "label",
                });
            }
            if !valid_single_line(&option.description, 0, MAX_OPTION_DESCRIPTION_CHARS) {
                return Err(InteractionError::InvalidOption {
                    index,
                    field: "description",
                });
            }
            if self.options[..index].iter().any(|candidate| {
                candidate
                    .label
                    .trim()
                    .eq_ignore_ascii_case(option.label.trim())
            }) {
                return Err(InteractionError::DuplicateOption { index });
            }
        }
        Ok(())
    }
}

fn valid_single_line(value: &str, minimum_chars: usize, maximum_chars: usize) -> bool {
    let trimmed = value.trim();
    let count = trimmed.chars().count();
    count >= minimum_chars
        && count <= maximum_chars
        && trimmed == value
        && !value.chars().any(char::is_control)
}
