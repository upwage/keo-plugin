//! Talking to Keo's MCP server: the base URL, the MCP client (JSON-RPC over streamable HTTP)
//! and the event contract (`contract`), kept in one module so a contract change is one edit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::auth::{Auth, AuthError};

/// Production. Used when the configured base URL is missing or not allowed.
pub const PROD_BASE: &str = "https://askkeo.ai";
const ALLOWED_HOST: &str = "askkeo.ai";
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// The timeout for one ordinary request to Keo (OAuth, update manifest). MCP calls get twice
/// this; the doorbell has its own, longer one for the long poll.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The origin of Keo (scheme + host), always https on askkeo.ai or one of its subdomains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl(String);

impl BaseUrl {
    /// Accept only `https://askkeo.ai` or `https://<sub>.askkeo.ai` (no port, no user info).
    pub fn parse_allowed(raw: &str) -> Option<Self> {
        let url = url::Url::parse(raw.trim()).ok()?;
        if url.scheme() != "https" || url.port().is_some() || !url.username().is_empty() {
            return None;
        }
        if url.password().is_some() {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        let ok = host == ALLOWED_HOST
            || host
                .strip_suffix(ALLOWED_HOST)
                .is_some_and(|sub| sub.ends_with('.') && sub.len() > 1);
        ok.then(|| BaseUrl(format!("https://{host}")))
    }

    /// Read `plugins.keo.baseUrl` from the app config; fall back to production.
    pub fn from_plugin_config(value: Option<&Value>) -> Self {
        let raw = value.and_then(|v| v.get("baseUrl")).and_then(Value::as_str);
        match raw.map(|r| (r, Self::parse_allowed(r))) {
            Some((_, Some(b))) => b,
            Some((_, None)) => {
                tracing::warn!(
                    "plugins.keo.baseUrl is not an allowed Keo origin; using production"
                );
                Self::prod()
            }
            None => Self::prod(),
        }
    }

    pub fn prod() -> Self {
        BaseUrl(PROD_BASE.to_string())
    }

    pub fn origin(&self) -> &str {
        &self.0
    }

    /// `path` must start with `/`.
    pub fn join(&self, path: &str) -> String {
        format!("{}{}", self.0, path)
    }

    pub fn mcp(&self) -> String {
        self.join("/api/mcp")
    }

    pub fn doorbell(&self) -> String {
        self.join("/api/mcp/doorbell")
    }

    /// Whether a URL the panel wants to open is on this origin.
    pub fn owns(&self, raw: &str) -> bool {
        url::Url::parse(raw).ok().is_some_and(|u| {
            u.scheme() == "https"
                && format!("https://{}", u.host_str().unwrap_or("")) == self.0
                && u.port().is_none()
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeoError {
    #[error("signed out")]
    SignedOut,
    /// The sign-in this call started under ended (sign out, or a new sign-in) before it
    /// finished. Its result belongs to nobody now: callers drop it and change no state.
    #[error("superseded by a newer sign-in")]
    Superseded,
    #[error("network: {0}")]
    Network(String),
    #[error("http status {0}")]
    Status(u16),
    #[error("protocol: {0}")]
    Protocol(String),
    /// Server text never rides in an error: errors reach the log, and a reply may echo
    /// someone's prompt. Only the JSON-RPC code is kept.
    #[error("rpc error {0}")]
    Rpc(i64),
    #[error("tool error ({0:?})")]
    Tool(contract::ErrorCode),
    /// The tool answered `unauthorized`: handled like a 401.
    #[error("unauthorized")]
    Unauthorized,
    /// The tool answered `rate_limited`: the caller backs off.
    #[error("rate limited")]
    RateLimited,
    #[error("auth: {0}")]
    Auth(String),
}

impl KeoError {
    /// A failure worth another try: the network, a server error, Keo asking to slow down, or
    /// Keo answering that it is unavailable.
    pub fn transient(&self) -> bool {
        match self {
            KeoError::Network(_)
            | KeoError::RateLimited
            | KeoError::Tool(contract::ErrorCode::KeoUnavailable) => true,
            KeoError::Status(s) => *s == 429 || *s >= 500,
            _ => false,
        }
    }
}

impl From<AuthError> for KeoError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::SignedOut => KeoError::SignedOut,
            AuthError::Superseded => KeoError::Superseded,
            other => KeoError::Auth(other.to_string()),
        }
    }
}

/// Why one attempt stopped, before the retry rules apply.
enum Attempt {
    Unauthorized,
    SessionGone,
    Done(Result<Value, KeoError>),
}

/// The MCP client for Keo. One MCP session, made on first use and remade when the server
/// forgets it.
/// Whether a call may still act: the sign-in it started under is the current one.
fn same_sign_in(started: u64, now: u64) -> bool {
    started == now
}

/// A finished call's result, or `Superseded` when the sign-in changed while it ran: a late
/// success must not mark the next account's machine registered, and a late `SignedOut` must
/// not sign the next account out.
fn fenced(started: u64, now: u64, result: Result<Value, KeoError>) -> Result<Value, KeoError> {
    if same_sign_in(started, now) {
        result
    } else {
        Err(KeoError::Superseded)
    }
}

pub struct KeoClient {
    base: BaseUrl,
    http: reqwest::Client,
    auth: Arc<Auth>,
    session: tokio::sync::Mutex<Option<Option<String>>>,
    next_id: AtomicU64,
    version: String,
}

impl KeoClient {
    /// The sign-in generation now (see `Auth::generation`).
    pub fn sign_in_generation(&self) -> u64 {
        self.auth.generation()
    }

    pub fn new(base: BaseUrl, http: reqwest::Client, auth: Arc<Auth>, version: String) -> Self {
        Self {
            base,
            http,
            auth,
            session: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
            version,
        }
    }

    /// Send one `coding_session_event`; succeeds only when Keo's reply is an object.
    /// Send one event under sign-in `generation`, the one it was accepted under: `Superseded`
    /// once that sign-in has ended, so a retry after a sign-out never sends one account's event
    /// with the next account's token.
    pub async fn send_event(
        &self,
        generation: u64,
        event: &contract::SessionEvent,
    ) -> Result<(), KeoError> {
        let reply = self
            .call_tool_at(generation, contract::TOOL_EVENT, event.to_args())
            .await?;
        if contract::event_reply_ok(&reply) {
            Ok(())
        } else {
            Err(KeoError::Protocol("event reply is not an object".into()))
        }
    }

    /// Forget the MCP session (after sign out).
    pub async fn reset(&self) {
        *self.session.lock().await = None;
    }

    /// Call one tool. A 401 refreshes the token once and retries once; a second 401 is
    /// `SignedOut`. A forgotten MCP session (404) is remade once.
    ///
    /// The call belongs to the sign-in it started under: if a sign-out or a new sign-in happens
    /// meanwhile, it never retries with the next account's token, and whatever it would have
    /// returned (success, `SignedOut`, any error) comes back as `Superseded` instead.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, KeoError> {
        self.call_tool_at(self.auth.generation(), name, arguments)
            .await
    }

    /// `call_tool` for a caller acting for sign-in `generation`.
    async fn call_tool_at(
        &self,
        generation: u64,
        name: &str,
        arguments: Value,
    ) -> Result<Value, KeoError> {
        let result = self.call_tool_once(generation, name, &arguments).await;
        fenced(generation, self.auth.generation(), result)
    }

    async fn call_tool_once(
        &self,
        generation: u64,
        name: &str,
        arguments: &Value,
    ) -> Result<Value, KeoError> {
        let mut refreshed = false;
        let mut remade = false;
        loop {
            let token = self.auth.access_token().await?;
            if !same_sign_in(generation, self.auth.generation()) {
                return Err(KeoError::Superseded);
            }
            let attempt = match self.attempt(&token, name, arguments).await {
                // A tool-level `unauthorized` takes the same path as an HTTP 401.
                Attempt::Done(Err(KeoError::Unauthorized)) => Attempt::Unauthorized,
                other => other,
            };
            match attempt {
                Attempt::Done(r) => return r,
                Attempt::Unauthorized if !refreshed => {
                    refreshed = true;
                    self.auth.invalidate(&token);
                    self.reset().await;
                }
                Attempt::Unauthorized => return Err(KeoError::SignedOut),
                Attempt::SessionGone if !remade => {
                    remade = true;
                    self.reset().await;
                }
                Attempt::SessionGone => {
                    return Err(KeoError::Protocol("MCP session lost twice".into()))
                }
            }
        }
    }

    async fn attempt(&self, token: &str, name: &str, arguments: &Value) -> Attempt {
        let mut session = self.session.lock().await;
        if session.is_none() {
            match self.initialize(token).await {
                Ok(id) => *session = Some(id),
                Err(a) => return a,
            }
        }
        let session_id = session.clone().flatten();
        drop(session);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        });
        match self.post(token, session_id.as_deref(), &body).await {
            Ok((401, _, _)) => Attempt::Unauthorized,
            Ok((404, _, _)) if session_id.is_some() => Attempt::SessionGone,
            Ok((status, _, text)) if (200..300).contains(&status) => {
                Attempt::Done(parse_rpc_reply(&text, id).and_then(tool_result))
            }
            Ok((status, _, _)) => Attempt::Done(Err(KeoError::Status(status))),
            Err(e) => Attempt::Done(Err(e)),
        }
    }

    /// `initialize` + `notifications/initialized`; returns the server's MCP session id.
    async fn initialize(&self, token: &str) -> Result<Option<String>, Attempt> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "keo-companion", "version": self.version },
            },
        });
        let (status, session_id, text) = match self.post(token, None, &body).await {
            Ok(r) => r,
            Err(e) => return Err(Attempt::Done(Err(e))),
        };
        if status == 401 {
            return Err(Attempt::Unauthorized);
        }
        if !(200..300).contains(&status) {
            return Err(Attempt::Done(Err(KeoError::Status(status))));
        }
        if let Err(e) = parse_rpc_reply(&text, id).and_then(|r| match r.get("error") {
            Some(err) => Err(KeoError::Rpc(contract::rpc_code(err))),
            None => Ok(()),
        }) {
            return Err(Attempt::Done(Err(e)));
        }
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        match self.post(token, session_id.as_deref(), &note).await {
            Ok((401, _, _)) => return Err(Attempt::Unauthorized),
            // A session the server never marked initialized is not cached.
            Ok((s, _, _)) if !(200..300).contains(&s) => {
                return Err(Attempt::Done(Err(KeoError::Status(s))))
            }
            Ok(_) => {}
            Err(e) => return Err(Attempt::Done(Err(e))),
        }
        Ok(session_id)
    }

    /// One POST to the MCP endpoint: (status, mcp-session-id header, body).
    async fn post(
        &self,
        token: &str,
        session_id: Option<&str>,
        body: &Value,
    ) -> Result<(u16, Option<String>, String), KeoError> {
        let mut req = self
            .http
            .post(self.base.mcp())
            .timeout(HTTP_TIMEOUT * 2)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .json(body);
        if let Some(sid) = session_id {
            req = req.header("mcp-session-id", sid);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| KeoError::Network(e.without_url().to_string()))?;
        let status = resp.status().as_u16();
        let sid = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp
            .text()
            .await
            .map_err(|e| KeoError::Network(e.without_url().to_string()))?;
        Ok((status, sid, text))
    }
}

/// Find the JSON-RPC response with `id` in a reply that is plain JSON or an SSE stream.
pub fn parse_rpc_reply(body: &str, id: u64) -> Result<Value, KeoError> {
    let trimmed = body.trim_start();
    let candidates: Vec<Value> = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => items,
            Ok(v) => vec![v],
            Err(e) => {
                return Err(KeoError::Protocol(format!(
                    "bad JSON reply ({:?})",
                    e.classify()
                )))
            }
        }
    } else {
        sse_data(body)
            .iter()
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect()
    };
    candidates
        .into_iter()
        .find(|v| v.get("id").and_then(Value::as_u64) == Some(id))
        .ok_or_else(|| KeoError::Protocol("no response for the request".into()))
}

/// The `data:` payload of each SSE event (multi-line data joined with `\n`).
pub fn sse_data(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in body.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n"));
                current.clear();
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            current.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n"));
    }
    out
}

/// A `tools/call` response to its value. Success needs `structuredContent` as an object, or
/// `content[0].text` holding JSON. A JSON-RPC `error`, `isError: true` (mapped by its
/// `<code>: <message>` text) and every other shape are errors, so a malformed reply never reads
/// as success. Each operation then checks its own success payload (`send_event`, `parse_pull`,
/// `ack_confirmed`).
pub fn tool_result(resp: Value) -> Result<Value, KeoError> {
    if let Some(err) = resp.get("error") {
        return Err(KeoError::Rpc(contract::rpc_code(err)));
    }
    let result = resp
        .get("result")
        .filter(|r| r.is_object())
        .ok_or_else(|| KeoError::Protocol("no result".into()))?;
    let text = result
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = text.unwrap_or("tool error");
        return Err(match contract::error_code(text) {
            contract::ErrorCode::Unauthorized => KeoError::Unauthorized,
            contract::ErrorCode::RateLimited => KeoError::RateLimited,
            code => KeoError::Tool(code),
        });
    }
    match result.get("structuredContent") {
        Some(sc) if sc.is_object() => return Ok(sc.clone()),
        Some(Value::Null) | None => {}
        Some(_) => {
            return Err(KeoError::Protocol(
                "structuredContent is not an object".into(),
            ))
        }
    }
    let text = text.ok_or_else(|| KeoError::Protocol("tool result has no content".into()))?;
    serde_json::from_str(text).map_err(|_| KeoError::Protocol("tool text is not JSON".into()))
}

/// The `coding_session_event` contract (v1.1 plus the v1.2 companion additions) and the inbox
/// tools. Everything that names a field or a cap lives here.
pub mod contract {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    pub const TOOL_EVENT: &str = "coding_session_event";
    pub const TOOL_INBOX_PULL: &str = "coding_session_inbox_pull";
    pub const TOOL_INBOX_ACK: &str = "coding_session_inbox_ack";
    pub const SOURCE: &str = "companion";

    pub const CAP_CWD: usize = 1000;
    pub const CAP_TEXT: usize = 16000;
    pub const CAP_PROGRESS: usize = 2000;
    pub const CAP_DETAIL: usize = 200;
    pub const CAP_PREVIEW: usize = 2000;

    /// The first `max` characters of `s` (never splits a character).
    /// The JSON-RPC error code, never its message.
    pub fn rpc_code(err: &Value) -> i64 {
        err.get("code").and_then(Value::as_i64).unwrap_or(0)
    }

    /// The OAuth error codes Keo may send (RFC 6749 sections 4.1.2.1 and 5.2, RFC 7591,
    /// RFC 8707). Anything else becomes `"other"`: free text from the server never reaches the
    /// log.
    const OAUTH_ERRORS: &[&str] = &[
        "access_denied",
        "invalid_client",
        "invalid_client_metadata",
        "invalid_grant",
        "invalid_redirect_uri",
        "invalid_request",
        "invalid_scope",
        "invalid_target",
        "server_error",
        "temporarily_unavailable",
        "unauthorized_client",
        "unsupported_grant_type",
        "unsupported_response_type",
    ];

    pub fn oauth_error(s: &str) -> String {
        OAUTH_ERRORS
            .iter()
            .find(|c| **c == s)
            .map_or("other", |c| c)
            .to_string()
    }

    pub fn cut(s: &str, max: usize) -> String {
        match s.char_indices().nth(max) {
            Some((i, _)) => s[..i].to_string(),
            None => s.to_string(),
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    pub enum EventKind {
        UserPromptSubmit,
        Stop,
        PermissionRequest,
        Notification,
        SessionStart,
        SessionEnd,
        PostToolUse,
        Register,
        Progress,
        Question,
        Usage,
        End,
    }

    /// One `coding_session_event` call's arguments. Unset fields are left out. Machine-level
    /// events (the machine Register, Usage) carry no `session_id` and no `agent`.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct SessionEvent {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        pub event: EventKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub agent: Option<&'static str>,
        pub source: &'static str,
        pub machine_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub task_id: Option<i64>,
        /// With `task_id` on a started Register: the lease of the pull that gave the task.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub lease: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub preview: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        /// Session states only: working | idle | finished | waiting_permission | started.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub state: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub prompt_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub options: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub window: Option<String>,
        /// 0 to 100.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub percent: Option<f64>,
        /// ISO 8601.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub resets_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        /// Machine Register only: the approved folders.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub approved_dirs: Option<Vec<String>>,
    }

    impl SessionEvent {
        fn base(event: EventKind, machine_id: &str) -> Self {
            SessionEvent {
                session_id: None,
                event,
                agent: None,
                source: SOURCE,
                machine_id: machine_id.to_string(),
                task_id: None,
                lease: None,
                cwd: None,
                text: None,
                detail: None,
                preview: None,
                tool: None,
                name: None,
                state: None,
                pid: None,
                prompt_id: None,
                options: None,
                window: None,
                percent: None,
                resets_at: None,
                reason: None,
                approved_dirs: None,
            }
        }

        /// An event about one session of one tool.
        pub fn session(
            session_id: &str,
            event: EventKind,
            agent: &'static str,
            machine_id: &str,
        ) -> Self {
            let mut e = Self::base(event, machine_id);
            e.session_id = Some(session_id.to_string());
            e.agent = Some(agent);
            e
        }

        /// The machine-only Register: no session id; `name`/`state` are ignored by Keo, so they
        /// are not sent.
        pub fn machine_register(
            machine_id: &str,
            tool: Option<&'static str>,
            approved_dirs: Vec<String>,
        ) -> Self {
            let mut e = Self::base(EventKind::Register, machine_id);
            e.tool = tool;
            e.approved_dirs = Some(approved_dirs);
            e
        }

        /// A usage reading for a tool on this machine (no session id).
        pub fn usage(
            machine_id: &str,
            tool: &'static str,
            window: &str,
            percent: f64,
            resets_at: Option<String>,
        ) -> Self {
            let mut e = Self::base(EventKind::Usage, machine_id);
            e.tool = Some(tool);
            e.window = Some(window.to_string());
            e.percent = Some(percent.clamp(0.0, 100.0));
            e.resets_at = resets_at;
            e
        }

        /// Apply the server's field caps.
        pub fn capped(mut self) -> Self {
            let text_cap = if self.event == EventKind::Progress {
                CAP_PROGRESS
            } else {
                CAP_TEXT
            };
            let cap = |v: &mut Option<String>, n: usize| {
                if let Some(s) = v.as_mut() {
                    *s = cut(s, n);
                }
            };
            cap(&mut self.cwd, CAP_CWD);
            cap(&mut self.text, text_cap);
            cap(&mut self.detail, CAP_DETAIL);
            cap(&mut self.preview, CAP_PREVIEW);
            cap(&mut self.reason, CAP_DETAIL);
            if let Some(dirs) = self.approved_dirs.as_mut() {
                for d in dirs.iter_mut() {
                    *d = cut(d, CAP_CWD);
                }
            }
            self
        }

        pub fn to_args(&self) -> Value {
            serde_json::to_value(self.clone().capped()).unwrap_or(Value::Null)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
    pub struct SlackThread {
        pub channel: String,
        pub thread_ts: String,
    }

    /// A task Keo approved in Slack. `Debug` leaves the text out, so it cannot reach a log.
    #[derive(Clone, PartialEq, Deserialize)]
    pub struct Task {
        pub task_id: i64,
        /// A fresh token on every pull; the task's ack must send it back.
        pub lease: String,
        /// The target session; `None` with `new: true` for a new session.
        #[serde(default)]
        pub session_id: Option<String>,
        #[serde(default)]
        pub new: bool,
        pub tool: String,
        pub cwd: String,
        pub text: String,
        #[serde(default)]
        pub slack_thread: Option<SlackThread>,
        #[serde(default)]
        pub approved_at: Option<String>,
    }

    impl std::fmt::Debug for Task {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Task")
                .field("task_id", &self.task_id)
                .field("session_id", &self.session_id)
                .field("new", &self.new)
                .field("tool", &self.tool)
                .field("text_chars", &self.text.chars().count())
                .finish()
        }
    }

    /// A person's answer to a session's `Question`. `Debug` leaves the answer text out.
    #[derive(Clone, PartialEq, Deserialize)]
    pub struct PromptAnswer {
        pub prompt_id: String,
        #[serde(default)]
        pub session_id: Option<String>,
        #[serde(default)]
        pub answer: Option<String>,
        /// The chosen option's index, as Keo sends it; its text is in `answer`.
        #[serde(default)]
        pub option: Option<u32>,
        #[serde(default)]
        pub cancelled: bool,
    }

    impl std::fmt::Debug for PromptAnswer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PromptAnswer")
                .field("prompt_id", &self.prompt_id)
                .field("session_id", &self.session_id)
                .field("cancelled", &self.cancelled)
                .finish()
        }
    }

    /// The pull reply: at most one task, and any answers waiting for this machine.
    #[derive(Debug, Clone, Default, PartialEq, Deserialize)]
    pub struct PullReply {
        #[serde(default)]
        pub task: Option<Task>,
        #[serde(default)]
        pub answers: Vec<PromptAnswer>,
    }

    /// Parse the pull result. It must be the `{task, answers}` object; any other shape is an
    /// error, so a malformed reply is never mistaken for "nothing to do".
    pub fn parse_pull(v: Value) -> Result<PullReply, String> {
        // Both documented fields, always: `{}` or an in-band `{"error": ...}` is not an empty pull.
        let Some(obj) = v.as_object() else {
            return Err("pull reply is not an object".into());
        };
        if !obj.contains_key("task") || !obj.get("answers").is_some_and(Value::is_array) {
            return Err("pull reply lacks task or answers".into());
        }
        // Serde's message quotes the offending value, which may be a prompt: keep only its kind.
        serde_json::from_value(v).map_err(|e| format!("bad pull reply ({:?})", e.classify()))
    }

    /// A `coding_session_event` reply counts as accepted only when it is an object.
    pub fn event_reply_ok(reply: &Value) -> bool {
        reply.is_object()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Outcome {
        Delivered,
        Started,
        Failed,
    }

    /// What an ack is for: exactly one of a task or a prompt answer.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub enum AckTarget {
        Task(i64),
        Prompt(String),
    }

    /// An ack reply confirms only with `ok: true` (`already: true` on a repeat is fine).
    pub fn ack_confirmed(reply: &Value) -> bool {
        reply.get("ok").and_then(Value::as_bool) == Some(true)
    }

    /// A task ack sends the lease of the pull that handed the task out; a prompt ack has none.
    pub fn ack_args(
        target: &AckTarget,
        lease: Option<&str>,
        outcome: Outcome,
        detail: Option<&str>,
    ) -> Value {
        let mut v = match target {
            AckTarget::Task(id) => serde_json::json!({ "task_id": id, "lease": lease }),
            AckTarget::Prompt(id) => serde_json::json!({ "prompt_id": id }),
        };
        v["outcome"] = serde_json::to_value(outcome).unwrap_or(Value::Null);
        if let Some(d) = detail {
            v["detail"] = Value::String(cut(d, CAP_DETAIL));
        }
        v
    }

    /// Pull by session, or by machine when the ring named no session.
    pub fn pull_args(session_id: Option<&str>, machine_id: &str) -> Value {
        match session_id {
            Some(s) => serde_json::json!({ "session_id": s }),
            None => serde_json::json!({ "machine_id": machine_id }),
        }
    }

    /// A tool error's code, from `content[0].text` = `<code>: <message>`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ErrorCode {
        InvalidArgument,
        NotYours,
        NotFound,
        RateLimited,
        Unauthorized,
        KeoUnavailable,
        Other,
    }

    pub fn error_code(text: &str) -> ErrorCode {
        match text.split(':').next().unwrap_or("").trim() {
            "invalid_argument" => ErrorCode::InvalidArgument,
            "not_yours" => ErrorCode::NotYours,
            "not_found" => ErrorCode::NotFound,
            "rate_limited" => ErrorCode::RateLimited,
            "unauthorized" => ErrorCode::Unauthorized,
            "keo_unavailable" => ErrorCode::KeoUnavailable,
            _ => ErrorCode::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::contract::*;
    use super::*;

    #[test]
    fn a_call_stops_when_the_sign_in_changes() {
        assert!(same_sign_in(3, 3));
        assert!(!same_sign_in(3, 4));
        // Same sign-in: the result passes through, whatever it is.
        assert_eq!(
            fenced(3, 3, Ok(json!({"ok": true}))).unwrap(),
            json!({"ok": true})
        );
        assert!(matches!(
            fenced(3, 3, Err(KeoError::SignedOut)),
            Err(KeoError::SignedOut)
        ));
        // Changed: a late success and a late SignedOut are both dropped.
        assert!(matches!(
            fenced(3, 4, Ok(json!({"ok": true}))),
            Err(KeoError::Superseded)
        ));
        assert!(matches!(
            fenced(3, 4, Err(KeoError::SignedOut)),
            Err(KeoError::Superseded)
        ));
    }

    #[test]
    fn base_url_rules() {
        assert_eq!(
            BaseUrl::parse_allowed("https://askkeo.ai")
                .unwrap()
                .origin(),
            "https://askkeo.ai"
        );
        assert_eq!(
            BaseUrl::parse_allowed("https://Beta.AskKeo.ai/some/path")
                .unwrap()
                .origin(),
            "https://beta.askkeo.ai"
        );
        for bad in [
            "http://askkeo.ai",
            "https://evilaskkeo.ai",
            "https://askkeo.ai.example.com",
            "https://askkeo.ai:8443",
            "https://.askkeo.ai",
            "not a url",
        ] {
            assert!(BaseUrl::parse_allowed(bad).is_none(), "{bad}");
        }
        // User info before the host is refused (built here so no address shape is in the source).
        let with_user = format!("https://user{}askkeo.ai", '@');
        assert!(BaseUrl::parse_allowed(&with_user).is_none());
        assert_eq!(BaseUrl::from_plugin_config(None), BaseUrl::prod());
        let cfg = json!({ "baseUrl": "https://example.com" });
        assert_eq!(BaseUrl::from_plugin_config(Some(&cfg)), BaseUrl::prod());
        let cfg = json!({ "baseUrl": "https://beta.askkeo.ai" });
        assert_eq!(
            BaseUrl::from_plugin_config(Some(&cfg)).mcp(),
            "https://beta.askkeo.ai/api/mcp"
        );
    }

    #[test]
    fn owns_only_its_origin() {
        let b = BaseUrl::prod();
        assert!(b.owns("https://askkeo.ai/manage"));
        assert!(!b.owns("https://example.com/"));
        assert!(!b.owns("http://askkeo.ai/"));
        assert!(!b.owns("https://beta.askkeo.ai/"));
    }

    #[test]
    fn parses_json_and_sse_replies() {
        let json_body = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        assert_eq!(parse_rpc_reply(json_body, 3).unwrap()["result"]["ok"], true);
        assert!(parse_rpc_reply(json_body, 4).is_err());

        let sse = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":7,\"result\":{\"x\":1}}\n\n";
        assert_eq!(parse_rpc_reply(sse, 7).unwrap()["result"]["x"], 1);
        assert!(parse_rpc_reply("data: not json\n\n", 1).is_err());
    }

    #[test]
    fn tool_results() {
        let structured = json!({"id":1,"result":{"structuredContent":{"a":1},"content":[{"type":"text","text":"{\"a\":2}"}]}});
        assert_eq!(tool_result(structured).unwrap()["a"], 1);
        let text_json = json!({"id":1,"result":{"content":[{"type":"text","text":"{\"a\":2}"}]}});
        assert_eq!(tool_result(text_json).unwrap()["a"], 2);
        let null_sc = json!({"id":1,"result":{"structuredContent":null,"content":[{"type":"text","text":"{\"a\":3}"}]}});
        assert_eq!(tool_result(null_sc).unwrap()["a"], 3);
        let rpc_err = json!({"id":1,"error":{"code":-32602,"message":"bad"}});
        assert!(matches!(tool_result(rpc_err), Err(KeoError::Rpc(_))));
        // Malformed envelopes are errors, never an empty success.
        for bad in [
            json!({"id":1,"result":{}}),
            json!({"id":1,"result":null}),
            json!({"id":1,"result":"ok"}),
            json!({"id":1,"result":{"content":"not an array"}}),
            json!({"id":1,"result":{"content":[]}}),
            json!({"id":1,"result":{"content":[{}]}}),
            json!({"id":1,"result":{"content":[{"type":"text","text":"not json"}]}}),
            json!({"id":1,"result":{"structuredContent":null}}),
            json!({"id":1,"result":{"structuredContent":[1,2]}}),
            json!({"id":1}),
        ] {
            assert!(
                matches!(tool_result(bad.clone()), Err(KeoError::Protocol(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn event_replies() {
        assert!(event_reply_ok(&json!({"ok": true})));
        assert!(event_reply_ok(&json!({})));
        assert!(!event_reply_ok(&Value::Null));
        assert!(!event_reply_ok(&json!("ok")));
        assert!(!event_reply_ok(&json!([])));
    }

    #[test]
    fn tool_error_codes() {
        let err = |text: &str| {
            tool_result(
                json!({"id":1,"result":{"isError":true,"content":[{"type":"text","text":text}]}}),
            )
        };
        assert!(matches!(
            err("unauthorized: token expired"),
            Err(KeoError::Unauthorized)
        ));
        assert!(matches!(
            err("rate_limited: slow down"),
            Err(KeoError::RateLimited)
        ));
        assert!(matches!(
            err("not_found: no task"),
            Err(KeoError::Tool(ErrorCode::NotFound))
        ));
        assert!(matches!(
            err("whatever"),
            Err(KeoError::Tool(ErrorCode::Other))
        ));
        assert_eq!(error_code("not_yours: nope"), ErrorCode::NotYours);
        assert_eq!(error_code("keo_unavailable"), ErrorCode::KeoUnavailable);
    }

    #[test]
    fn ack_must_confirm() {
        assert!(ack_confirmed(&json!({"ok": true, "already": false})));
        assert!(ack_confirmed(&json!({"ok": true, "already": true})));
        // An echo of the id is not a confirmation any more.
        assert!(!ack_confirmed(&json!({"task_id": 7})));
        assert!(!ack_confirmed(&json!({"ok": false})));
        assert!(!ack_confirmed(&Value::Null));
        assert!(!ack_confirmed(&json!({})));
        assert!(!ack_confirmed(&json!("ok")));
    }

    #[test]
    fn server_text_never_reaches_an_error() {
        const SECRET: &str = "private prompt text";
        let pull = parse_pull(json!({"task": SECRET, "answers": []})).unwrap_err();
        assert!(!pull.contains(SECRET), "{pull}");
        let rpc = tool_result(json!({"error": {"code": -32000, "message": SECRET}})).unwrap_err();
        assert!(!rpc.to_string().contains(SECRET));
        let tool = tool_result(json!({"result": {"isError": true,
            "content": [{"type": "text", "text": format!("not_found: {SECRET}")}]}}))
        .unwrap_err();
        assert!(!tool.to_string().contains(SECRET));
        let bad = parse_rpc_reply(&format!("{{\"x\": {SECRET}}}"), 1).unwrap_err();
        assert!(!bad.to_string().contains(SECRET));
        assert_eq!(contract::oauth_error("access_denied"), "access_denied");
        assert_eq!(contract::oauth_error(SECRET), "other");
        assert_eq!(contract::oauth_error("private_prompt_text"), "other");
    }

    #[test]
    fn cut_respects_char_boundaries() {
        assert_eq!(cut("hello", 3), "hel");
        assert_eq!(cut("hi", 10), "hi");
        assert_eq!(cut("ééé", 2), "éé");
        assert_eq!(cut("a😀b", 2), "a😀");
        assert_eq!(cut("", 0), "");
    }

    #[test]
    fn event_caps_apply() {
        let mut e = SessionEvent::session("sess-0001", EventKind::Progress, "codex", "machine-01");
        e.text = Some("x".repeat(5000));
        e.detail = Some("d".repeat(500));
        e.cwd = Some("/".repeat(2000));
        let v = e.to_args();
        assert_eq!(v["text"].as_str().unwrap().len(), CAP_PROGRESS);
        assert_eq!(v["detail"].as_str().unwrap().len(), CAP_DETAIL);
        assert_eq!(v["cwd"].as_str().unwrap().len(), CAP_CWD);
        assert_eq!(v["source"], "companion");
        assert_eq!(v["event"], "Progress");
        assert_eq!(v["session_id"], "sess-0001");
        assert!(v.get("preview").is_none());

        let mut stop =
            SessionEvent::session("sess-0001", EventKind::Stop, "claude-code", "machine-01");
        stop.text = Some("y".repeat(20000));
        assert_eq!(stop.to_args()["text"].as_str().unwrap().len(), CAP_TEXT);
    }

    #[test]
    fn machine_events_omit_session_id() {
        let reg = SessionEvent::machine_register("machine-01", Some("codex"), vec!["/w".into()])
            .to_args();
        assert!(reg.get("session_id").is_none());
        assert!(reg.get("agent").is_none());
        assert!(reg.get("name").is_none());
        assert!(reg.get("state").is_none());
        assert_eq!(reg["event"], "Register");
        assert_eq!(reg["tool"], "codex");
        assert_eq!(reg["approved_dirs"][0], "/w");
        assert_eq!(reg["source"], "companion");

        let usage = SessionEvent::usage(
            "machine-01",
            "codex",
            "5h",
            140.0,
            Some("2026-11-01T20:00:00Z".into()),
        )
        .to_args();
        assert!(usage.get("session_id").is_none());
        assert_eq!(usage["percent"], 100.0);
        assert_eq!(usage["event"], "Usage");
        assert_eq!(usage["resets_at"], "2026-11-01T20:00:00Z");
    }

    #[test]
    fn pull_parsing() {
        assert!(parse_pull(Value::Null).is_err());
        assert_eq!(
            parse_pull(json!({"task": null, "answers": []})).unwrap(),
            PullReply::default()
        );
        assert!(parse_pull(json!("nothing")).is_err());
        assert!(parse_pull(json!({})).is_err());
        assert!(parse_pull(json!({"error": "unavailable"})).is_err());
        assert!(parse_pull(json!({"task": null})).is_err());
        let reply = parse_pull(json!({
            "task": {"task_id": 42, "lease": "l-42", "session_id": null, "new": true, "tool": "codex", "cwd": "/w",
                     "text": "do it", "slack_thread": {"channel": "chan", "thread_ts": "1.2"},
                     "approved_at": "2026-01-01T00:00:00Z"},
            "answers": [{"prompt_id": "p1", "session_id": "sess1234", "answer": null,
                         "option": 1, "cancelled": false}]
        }))
        .unwrap();
        let task = reply.task.unwrap();
        assert_eq!(task.task_id, 42);
        assert_eq!(task.lease, "l-42");
        assert!(task.new);
        assert_eq!(task.session_id, None);
        assert_eq!(task.slack_thread.unwrap().thread_ts, "1.2");
        assert_eq!(reply.answers[0].option, Some(1));
        // A string task id is not the contract.
        assert!(parse_pull(
            json!({"task": {"task_id": "t1", "tool": "codex", "cwd": "/w", "text": ""}})
        )
        .is_err());
    }

    #[test]
    fn ack_and_pull_args() {
        let a = ack_args(
            &AckTarget::Task(7),
            Some("lease-7"),
            Outcome::Failed,
            Some(&"z".repeat(300)),
        );
        assert_eq!(a["task_id"], 7);
        assert_eq!(a["lease"], "lease-7");
        assert!(a.get("prompt_id").is_none());
        assert_eq!(a["outcome"], "failed");
        assert_eq!(a["detail"].as_str().unwrap().len(), CAP_DETAIL);
        let p = ack_args(
            &AckTarget::Prompt("p1".into()),
            None,
            Outcome::Delivered,
            None,
        );
        assert_eq!(p["prompt_id"], "p1");
        assert!(p.get("lease").is_none());
        assert!(p.get("task_id").is_none());
        assert!(p.get("detail").is_none());
        assert_eq!(pull_args(Some("s1234567"), "m")["session_id"], "s1234567");
        assert_eq!(pull_args(None, "machine-01")["machine_id"], "machine-01");
    }
}
