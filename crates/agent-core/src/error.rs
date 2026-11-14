//! Error types shared by the runtime.

use thiserror::Error;

/// Errors that can stop an agent run before it produces a useful outcome.
#[derive(Debug, Clone, Error)]
pub enum AgentError {
    #[error("the agent is already running a turn")]
    Busy,
    #[error("the prompt is empty")]
    EmptyPrompt,
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("unknown tool `{tool}`")]
    UnknownTool { tool: String },
    #[error("tool `{tool}` was denied by the user")]
    Denied { tool: String },
    #[error("{0}")]
    Other(String),
}

/// Errors raised while talking to a model provider.
///
/// The distinction between retryable and fatal errors matters: the harness
/// retries only when it has not received any streamed output yet, so a retry
/// never duplicates visible text.
#[derive(Debug, Clone, Error)]
pub enum ProviderError {
    #[error("request cancelled")]
    Cancelled,
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("API error: {0}")]
    Api(String),
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Network(_) => true,
            ProviderError::Http { status, .. } => {
                matches!(*status, 408 | 409 | 425 | 429) || (500..=599).contains(status)
            }
            ProviderError::Api(message) => {
                let lower = message.to_ascii_lowercase();
                lower.contains("overload")
                    || lower.contains("rate limit")
                    || lower.contains("timeout")
                    || lower.contains("temporarily")
            }
            ProviderError::Cancelled | ProviderError::Parse(_) | ProviderError::Other(_) => false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, ProviderError::Cancelled)
    }
}

/// A tool execution failure. Tool failures are *data*, not harness failures:
/// they are appended to the transcript so the model can recover.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<std::io::Error> for ToolError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}
