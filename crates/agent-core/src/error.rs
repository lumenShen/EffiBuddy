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

    /// True when the provider refused the request only because of rate
    /// limiting (HTTP 429, "too many requests", an overload/capacity notice).
    ///
    /// The harness treats this class as open ended: the request itself is
    /// valid and the limit always clears eventually, so counting attempts and
    /// surfacing "HTTP 429" after two of them just turns a temporary limit
    /// into a dead turn the user has to re-send by hand. Every other retryable
    /// failure keeps the bounded retry budget.
    pub fn is_rate_limited(&self) -> bool {
        match self {
            // 429 is the canonical "come back later". 503/529 are what the
            // OpenAI and Anthropic gateways use for overload, but they also
            // cover real outages, so they only count when the body says the
            // provider is shedding load.
            ProviderError::Http { status, body } => {
                *status == 429 || (matches!(*status, 503 | 529) && mentions_rate_limit(body))
            }
            ProviderError::Api(message) => mentions_rate_limit(message),
            ProviderError::Cancelled
            | ProviderError::Network(_)
            | ProviderError::Parse(_)
            | ProviderError::Other(_) => false,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, ProviderError::Cancelled)
    }

    /// True when the provider rejected the request because the prompt no
    /// longer fits its context window. The harness uses this to run one
    /// reactive compaction and retry instead of surfacing a dead turn.
    pub fn is_context_overflow(&self) -> bool {
        let text = match self {
            ProviderError::Http { status, body } => {
                if !matches!(*status, 400 | 413 | 422) {
                    return false;
                }
                body.as_str()
            }
            ProviderError::Api(message) | ProviderError::Other(message) => message.as_str(),
            ProviderError::Cancelled | ProviderError::Network(_) | ProviderError::Parse(_) => {
                return false;
            }
        };
        let lower = text.to_ascii_lowercase();
        [
            "context length",
            "context_length",
            "context window",
            "maximum context",
            "max context",
            "prompt is too long",
            "input is too long",
            "conversation is too long",
            "too many tokens",
            "token limit",
            "maximum number of tokens",
            "request too large",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    }
}

/// Rate-limit / load-shedding phrasing shared by the provider adapters.
fn mentions_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "rate limit",
        "rate_limit",
        "ratelimit",
        "too many requests",
        "overloaded",
        "at capacity",
        "over capacity",
        "quota exceeded",
        "slow down",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_are_classified_separately_from_other_failures() {
        assert!(
            ProviderError::Http {
                status: 429,
                body: "Rate limit exceeded. Please try again later.".to_string(),
            }
            .is_rate_limited()
        );
        assert!(
            ProviderError::Http {
                status: 503,
                body: "{\"error\":{\"type\":\"overloaded_error\"}}".to_string(),
            }
            .is_rate_limited()
        );
        assert!(
            !ProviderError::Http {
                status: 500,
                body: "internal error".to_string(),
            }
            .is_rate_limited()
        );
        assert!(
            !ProviderError::Http {
                status: 400,
                body: "bad request".to_string(),
            }
            .is_rate_limited()
        );
        assert!(ProviderError::Api("rate limit reached".to_string()).is_rate_limited());
        assert!(!ProviderError::Network("connection reset".to_string()).is_rate_limited());
    }
}
