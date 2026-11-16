//! Chrome DevTools Protocol transport.
//!
//! One WebSocket carries every domain for one target. Requests are correlated
//! by `id`; events fan out to subscribers keyed by method name.
//!
//! Design notes:
//!
//! - A single writer task owns the sink, fed by an mpsc channel, so `call` never
//!   holds a lock across an `await`.
//! - A single reader task correlates replies and broadcasts events. When the
//!   socket closes, **every** pending call is failed with a protocol error
//!   instead of hanging: a hung tool call would block the agent run forever.
//! - `CdpConnection` is cheap to share behind an `Arc` and safe to call
//!   concurrently; Chromium tolerates interleaved requests on one connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::types::BrowserError;

pub type EventSender = broadcast::Sender<CdpEvent>;

/// How long one protocol call may take before it is failed.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const OUTBOUND_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    /// Flattened `sessionId`, for page-scoped events on a browser-level socket.
    pub session_id: Option<String>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, BrowserError>>>>>;

/// A connected CDP endpoint.
pub struct CdpConnection {
    next_id: AtomicI64,
    pending: Pending,
    events: EventSender,
    outbound: mpsc::Sender<Message>,
    closed: Arc<AtomicBool>,
}

impl CdpConnection {
    /// Connect to a `ws://` DevTools URL and start pumping frames.
    pub async fn connect(url: &str) -> Result<Arc<Self>, BrowserError> {
        let (stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| {
                BrowserError::Protocol(format!("websocket connect failed: {error}"))
            })?;

        let (mut sink, mut stream) = stream.split();
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_CHANNEL_CAPACITY);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        // Writer task: owns the sink.
        tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Reader task: correlates replies, broadcasts events, fails pending on
        // close.
        let reader_pending = pending.clone();
        let reader_events = event_tx.clone();
        let reader_closed = closed.clone();
        tokio::spawn(async move {
            while let Some(incoming) = stream.next().await {
                match incoming {
                    Ok(Message::Text(text)) => {
                        handle_frame(&text, &reader_pending, &reader_events);
                    }
                    Ok(Message::Binary(bytes)) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            handle_frame(text, &reader_pending, &reader_events);
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                    Err(_) => break,
                }
            }
            reader_closed.store(true, Ordering::SeqCst);
            fail_all_pending(&reader_pending);
        });

        Ok(Arc::new(Self {
            next_id: AtomicI64::new(1),
            pending,
            events: event_tx,
            outbound: outbound_tx,
            closed,
        }))
    }

    /// Send a request and await its reply.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.call_inner(method, params, None).await
    }

    /// Send a request scoped to a page session attached with
    /// `Target.attachToTarget`.
    pub async fn call_session(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, BrowserError> {
        self.call_inner(method, params, Some(session_id)).await
    }

    async fn call_inner(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, BrowserError> {
        if self.is_closed() {
            return Err(BrowserError::Protocol(
                "devtools connection is closed".to_string(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, tx);

        let mut frame = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            frame["sessionId"] = json!(session_id);
        }
        let text = Message::Text(frame.to_string().into());
        if self.outbound.send(text).await.is_err() {
            self.forget(id);
            return Err(BrowserError::Protocol(
                "devtools connection is closed".to_string(),
            ));
        }

        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(BrowserError::Protocol(
                "devtools reply channel was dropped".to_string(),
            )),
            Err(_) => {
                self.forget(id);
                Err(BrowserError::Timeout(format!(
                    "devtools call `{method}` timed out"
                )))
            }
        }
    }

    fn forget(&self, id: i64) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Assign the next request id. Exposed for tests.
    pub fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn handle_frame(text: &str, pending: &Pending, events: &EventSender) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };

    if let Some(id) = value.get("id").and_then(Value::as_i64) {
        let sender = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
        let Some(sender) = sender else { return };
        let result = if let Some(error) = value.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown devtools error");
            Err(BrowserError::Protocol(format!("{code}: {message}")))
        } else {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = sender.send(result);
        return;
    }

    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    let event = CdpEvent {
        method: method.to_string(),
        params: value.get("params").cloned().unwrap_or(Value::Null),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let _ = events.send(event);
}

fn fail_all_pending(pending: &Pending) {
    let drained: Vec<_> = pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .collect();
    for (_, sender) in drained {
        let _ = sender.send(Err(BrowserError::Protocol(
            "devtools connection closed".to_string(),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::net::TcpListener;

    /// A minimal DevTools-like server: answers each request after a delay that
    /// varies with the id, so replies can arrive in a different order.
    async fn spawn_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text).unwrap();
                let id = value["id"].as_i64().unwrap();
                let method = value["method"].as_str().unwrap().to_string();
                let delay = 120u64.saturating_sub((id as u64 % 5) * 25);
                if method == "Emit.Event" {
                    let event = json!({
                        "method": "Page.loadEventFired",
                        "params": {"timestamp": 1.0},
                        "sessionId": "page-1",
                    });
                    socket
                        .send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                if method == "Fail.Call" {
                    let error = json!({
                        "id": id,
                        "error": {"code": -32601, "message": "method not found"},
                    });
                    socket
                        .send(Message::Text(error.to_string().into()))
                        .await
                        .unwrap();
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let reply = json!({"id": id, "result": {"echo": method}});
                if socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        format!("ws://127.0.0.1:{port}/devtools/browser/test")
    }

    #[tokio::test]
    async fn concurrent_calls_are_correlated_by_id_even_when_replies_interleave() {
        let url = spawn_server().await;
        let connection = CdpConnection::connect(&url).await.unwrap();
        let (a, b, c) = tokio::join!(
            connection.call("A.method", json!({})),
            connection.call("B.method", json!({})),
            connection.call("C.method", json!({})),
        );
        assert_eq!(a.unwrap()["echo"], "A.method");
        assert_eq!(b.unwrap()["echo"], "B.method");
        assert_eq!(c.unwrap()["echo"], "C.method");
    }

    #[tokio::test]
    async fn protocol_errors_surface_code_and_message() {
        let url = spawn_server().await;
        let connection = CdpConnection::connect(&url).await.unwrap();
        let error = connection
            .call("Fail.Call", json!({}))
            .await
            .expect_err("must fail");
        match error {
            BrowserError::Protocol(message) => {
                assert!(message.contains("-32601"), "{message}");
                assert!(message.contains("method not found"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn events_are_broadcast_with_session_id() {
        let url = spawn_server().await;
        let connection = CdpConnection::connect(&url).await.unwrap();
        let mut events = connection.subscribe();
        let call = tokio::spawn({
            let connection = connection.clone();
            async move { connection.call("Emit.Event", json!({})).await }
        });
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event timed out")
            .expect("event channel");
        assert_eq!(event.method, "Page.loadEventFired");
        assert_eq!(event.session_id.as_deref(), Some("page-1"));
        call.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn pending_calls_fail_instead_of_hanging_when_the_socket_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Read one request then drop the socket without replying.
            let _ = socket.next().await;
            drop(socket);
        });

        let connection = CdpConnection::connect(&format!("ws://127.0.0.1:{port}/x"))
            .await
            .unwrap();
        let started = Instant::now();
        let error = connection
            .call("Page.navigate", json!({}))
            .await
            .expect_err("must fail");
        assert!(
            matches!(error, BrowserError::Protocol(_)),
            "unexpected error: {error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(connection.is_closed());
        // Later calls fail fast rather than waiting for a dead socket.
        assert!(connection.call("Page.reload", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn ids_are_monotonic() {
        let url = spawn_server().await;
        let connection = CdpConnection::connect(&url).await.unwrap();
        let first = connection.next_id();
        let second = connection.next_id();
        assert_eq!(second, first + 1);
    }
}
