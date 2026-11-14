//! Test doubles for the runtime.
//!
//! Enabled by `cfg(test)` or the `testing` feature so downstream crates can
//! build deterministic harness tests without a network provider.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::ProviderError;
use crate::provider::{
    ChatRequest, Completion, ModelProvider, ProviderEvent, chunk_chars, send_event,
};

/// A provider that replays a fixed script of completions.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Completion>>,
    requests: Mutex<Vec<ChatRequest>>,
    chunk_size: usize,
    delay: Duration,
}

impl ScriptedProvider {
    pub fn new(script: Vec<Completion>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            chunk_size: 3,
            delay: Duration::from_millis(4),
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn context_window(&self) -> usize {
        32_768
    }

    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
        events: UnboundedSender<ProviderEvent>,
    ) -> Result<Completion, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let completion =
            self.script.lock().unwrap().pop_front().ok_or_else(|| {
                ProviderError::Other("scripted provider ran out of responses".into())
            })?;

        for chunk in chunk_chars(&completion.reasoning, self.chunk_size) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            send_event(&events, ProviderEvent::ReasoningDelta(chunk))?;
            tokio::time::sleep(self.delay).await;
        }
        for chunk in chunk_chars(&completion.text, self.chunk_size) {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            send_event(&events, ProviderEvent::TextDelta(chunk))?;
            tokio::time::sleep(self.delay).await;
        }
        Ok(completion)
    }
}
