//! JSON-RPC 2.0 over the app-server's loopback websocket. Every request has a timeout: the
//! spike saw a second `turn/interrupt` that never answered. A `-32001` (server overloaded)
//! answer is retried with jittered backoff.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

/// The app-server's "server overloaded, retry later" code.
pub const OVERLOADED: i64 = -32001;
/// Tries for a request the server answers with `OVERLOADED`.
const OVERLOAD_TRIES: u32 = 5;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RpcError {
    #[error("codex error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("codex did not answer {0} in time")]
    Timeout(String),
    #[error("the codex connection closed")]
    Closed,
    #[error("could not connect to codex: {0}")]
    Connect(String),
}

/// What the server sends that is not an answer to one of ours.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Notification {
        method: String,
        params: Value,
    },
    /// A server request: it waits for `Conn::respond` with the same id.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

type Pending = Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>;

pub struct Conn {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicI64,
    closed: AtomicBool,
}

impl Conn {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send a request and wait for its answer, retrying `OVERLOADED` with jittered backoff.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let mut attempt = 0;
        loop {
            match self.request(method, params.clone(), timeout).await {
                Err(RpcError::Server { code, .. }) if code == OVERLOADED => {
                    attempt += 1;
                    if attempt >= OVERLOAD_TRIES {
                        return Err(RpcError::Server {
                            code,
                            message: "codex is overloaded".into(),
                        });
                    }
                    let base = 250u64 << attempt;
                    let jitter = rand::thread_rng().gen_range(0..=base / 2);
                    tracing::info!(method, attempt, "codex overloaded; backing off");
                    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
                }
                other => return other,
            }
        }
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.out.send(Message::text(frame.to_string())).is_err() {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(RpcError::Closed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(RpcError::Timeout(method.to_string()))
            }
        }
    }

    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        let mut frame = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            frame["params"] = p;
        }
        self.out
            .send(Message::text(frame.to_string()))
            .map_err(|_| RpcError::Closed)
    }

    /// Answer a server request.
    pub fn respond(&self, id: &Value, result: Value) -> Result<(), RpcError> {
        if self.is_closed() {
            return Err(RpcError::Closed);
        }
        let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
        self.out
            .send(Message::text(frame.to_string()))
            .map_err(|_| RpcError::Closed)
    }

    /// Refuse a server request this client does not handle.
    pub fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<(), RpcError> {
        let frame =
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        self.out
            .send(Message::text(frame.to_string()))
            .map_err(|_| RpcError::Closed)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        for (_, tx) in self.pending.lock().expect("pending lock").drain() {
            let _ = tx.send(Err(RpcError::Closed));
        }
    }

    /// Route one text frame: an answer completes its request; anything else goes to `inbound`.
    fn route(&self, text: &str, inbound: &mpsc::UnboundedSender<Inbound>) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            tracing::warn!("codex sent a frame that is not JSON");
            return;
        };
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let msg = match v.get("id") {
                Some(id) if !id.is_null() => Inbound::Request {
                    id: id.clone(),
                    method: method.to_string(),
                    params,
                },
                _ => Inbound::Notification {
                    method: method.to_string(),
                    params,
                },
            };
            let _ = inbound.send(msg);
            return;
        }
        let Some(id) = v.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let answer = match v.get("error") {
            Some(e) => Err(RpcError::Server {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            }),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(answer);
    }
}

/// Open the websocket with the capability token. The receiver ends when the connection closes.
pub async fn connect(
    url: &str,
    token: &str,
) -> Result<(Arc<Conn>, mpsc::UnboundedReceiver<Inbound>), RpcError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| RpcError::Connect(e.to_string()))?;
    let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|e| RpcError::Connect(e.to_string()))?;
    request.headers_mut().insert("Authorization", bearer);
    let (ws, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| RpcError::Connect("timed out".into()))?
    .map_err(|e| RpcError::Connect(e.to_string()))?;
    let (mut sink, mut stream) = ws.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let (inbound, inbound_rx) = mpsc::unbounded_channel();
    let conn = Arc::new(Conn {
        out,
        pending: Mutex::new(HashMap::new()),
        next: AtomicI64::new(1),
        closed: AtomicBool::new(false),
    });
    tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let reader = Arc::clone(&conn);
    tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(t)) => reader.route(t.as_str(), &inbound),
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        reader.close();
        drop(inbound);
    });
    Ok((conn, inbound_rx))
}
