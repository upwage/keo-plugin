//! The Codex runner. It drives the companion's own `codex app-server` over a loopback
//! websocket (`server.rs` starts it; `rpc.rs` speaks JSON-RPC; `map.rs` turns
//! frames into runner events). Frame shapes come from the spike recordings in
//! `docs/work/2026-11-01/spike/` (codex-cli 0.158.0, the oldest supported version).
//!
//! - Roster: `thread/list` (recent, not archived) with each thread's real `cwd`; a thread this
//!   runner touched that fell off the page is read with `thread/read`.
//! - Start: `thread/start {cwd, approvalPolicy:"on-request", sandbox:"workspace-write"}`, then
//!   `thread/name/set`; the thread stays idle and off the roster until its first `send`.
//! - Send: `thread/resume` when the thread is not loaded here, then `turn/start` (or
//!   `turn/steer` when a turn is running). Steer: `turn/steer {expectedTurnId}`. Stop:
//!   `turn/interrupt`.
//! - Server requests (approvals, user input, MCP elicitation) become `Question`s with a fresh v4
//!   `prompt_id` each; `answer()` replies. No answer in time declines.
//! - Notifications become Progress, TurnEnded, Usage, SessionEnded and RosterChanged.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

use super::{
    detect_tool, Answer, Detection, RosterEntry, Runner, RunnerError, RunnerEvent, StartRequest,
    ToolKind, ToolStatus,
};

mod map;
mod rpc;
mod server;
#[cfg(test)]
mod tests;

use map::{Ask, Reply};
use rpc::{Conn, Inbound, RpcError};

const KIND: ToolKind = ToolKind::Codex;

/// Threads the roster lists: the newest page, touched within this many days.
const ROSTER_PAGE: u32 = 50;
const ROSTER_DAYS: i64 = 14;
/// Bundled Codex plugins that drive the screen or a browser. A Keo-started session works from
/// Slack with nobody at the machine, so it runs without them: a file edit stays a command, not
/// clicks in TextEdit with one approval per step.
const GUI_PLUGINS: &[&str] = &[
    "computer-use@openai-bundled",
    "unified-computer-use@openai-bundled",
    "chrome@openai-bundled",
    "browser@openai-bundled",
];

/// `thread/start` config overrides (dotted keys, as `-c` takes them): the GUI plugins off, the
/// rest of the person's config as it is.
fn lean_config() -> Value {
    Value::Object(
        GUI_PLUGINS
            .iter()
            .map(|p| (format!("plugins.{p}.enabled"), Value::Bool(false)))
            .collect(),
    )
}

/// JSON-RPC internal error: the server failed, not the thread.
const INTERNAL_ERROR: i64 = -32603;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// `turn/interrupt` gets a short timeout: a repeated interrupt is never answered.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Keo cancels an unanswered question at 30 minutes; this local limit covers a Keo that never
/// says so (offline, signed out).
const QUESTION_TIMEOUT: Duration = Duration::from_secs(35 * 60);
/// Prompt ids already settled, kept so a late answer to one is a no-op, not an error.
const SETTLED_KEEP: usize = 256;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where the token, port and app-server log live: the app data folder, set once at startup.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

fn data_dir() -> PathBuf {
    DATA_DIR.get().cloned().unwrap_or_else(|| {
        tracing::warn!("codex: no app data folder set; using the temp folder");
        std::env::temp_dir().join("keo-companion")
    })
}

/// How the runner reaches an app-server.
enum Launch {
    /// Attach to the server an earlier launch started, else start one.
    Auto,
    /// A fixed endpoint (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed { url: String, token: String },
}

/// A server request waiting for its answer.
struct Prompt {
    generation: u64,
    request_id: Value,
    session_id: String,
    ask: Ask,
    /// The `requestUserInput` this question is one part of.
    group: Option<String>,
}

/// The answers of a multi-question `requestUserInput`, sent once every question has one.
struct Group {
    answers: Map<String, Value>,
    open: HashSet<String>,
}

/// What the notifications say about one thread.
#[derive(Default)]
struct Live {
    /// The connection generation that has the thread loaded (0: not loaded).
    loaded: u64,
    turn: Option<String>,
    /// The last turn that completed, so a late `turn/start` reply cannot revive it.
    done_turn: Option<String>,
    status: Option<String>,
    last_message: Option<String>,
    /// Files an `item/started` file change named, by item id, for the approval's text.
    changes: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct State {
    threads: HashMap<String, Live>,
    prompts: HashMap<String, Prompt>,
    groups: HashMap<String, Group>,
    settled: Vec<String>,
    usage: HashMap<String, (f64, Option<String>)>,
    /// Threads this runner started or sent to; listed even when off the roster page.
    touched: HashSet<String>,
    /// Threads `start` opened whose first `send` has not happened: left off the roster, so the
    /// inbox's Register is the first Keo hears of them. The flag says the name may still take a
    /// ticket key from the first words.
    unsent: HashMap<String, bool>,
}

struct Inner {
    events: broadcast::Sender<RunnerEvent>,
    launch: Launch,
    conn: tokio::sync::Mutex<Option<(u64, Arc<Conn>)>>,
    child: Mutex<Option<server::Server>>,
    generation: AtomicU64,
    question_timeout: Duration,
    st: Mutex<State>,
    /// `start` holds it for writing from `thread/start` until the new id is in `unsent`; `list`
    /// holds it for reading from `thread/list` until it has read `unsent`. So a roster read never
    /// sees a new thread before it is marked unsent (that would put it on Keo's roster ahead of
    /// the inbox's Register, and Keo would keep its answers at the machine).
    creating: tokio::sync::RwLock<()>,
}

pub struct CodexRunner {
    inner: Arc<Inner>,
}

impl CodexRunner {
    pub fn new() -> Self {
        Self::build(Launch::Auto, QUESTION_TIMEOUT)
    }

    fn build(launch: Launch, question_timeout: Duration) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                events,
                launch,
                conn: tokio::sync::Mutex::new(None),
                child: Mutex::new(None),
                generation: AtomicU64::new(0),
                question_timeout,
                st: Mutex::new(State::default()),
                creating: tokio::sync::RwLock::new(()),
            }),
        }
    }

    #[cfg(test)]
    fn fixed(url: String, token: String, question_timeout: Duration) -> Self {
        Self::build(Launch::Fixed { url, token }, question_timeout)
    }
}

impl Default for CodexRunner {
    fn default() -> Self {
        Self::new()
    }
}

fn failed(e: RpcError) -> RunnerError {
    RunnerError::Failed(e.to_string())
}

fn text_input(text: &str) -> Value {
    json!([{"type": "text", "text": text, "text_elements": []}])
}

impl Inner {
    fn emit(&self, ev: RunnerEvent) {
        let _ = self.events.send(ev);
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().expect("codex state lock")
    }

    /// The live connection, made (and the server started) when there is none.
    async fn conn(self: &Arc<Self>) -> Result<(u64, Arc<Conn>), RunnerError> {
        let mut guard = self.conn.lock().await;
        if let Some((g, c)) = guard.as_ref() {
            if !c.is_closed() {
                return Ok((*g, Arc::clone(c)));
            }
        }
        let (conn, inbound) = self.open().await?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let init = conn
            .call(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "keo-companion",
                        "title": "Keo Companion",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {"experimentalApi": true},
                }),
                CALL_TIMEOUT,
            )
            .await
            .map_err(failed)?;
        conn.notify("initialized", None).map_err(failed)?;
        let user_agent = init.get("userAgent").and_then(Value::as_str).unwrap_or("");
        tracing::info!(user_agent, "codex: connected");
        let me = Arc::downgrade(self);
        let pump_conn = Arc::clone(&conn);
        tokio::spawn(pump(me, generation, pump_conn, inbound));
        *guard = Some((generation, Arc::clone(&conn)));
        Ok((generation, conn))
    }

    async fn open(
        &self,
    ) -> Result<(Arc<Conn>, tokio::sync::mpsc::UnboundedReceiver<Inbound>), RunnerError> {
        let (url, token) = match &self.launch {
            Launch::Fixed { url, token } => {
                return rpc::connect(url, token).await.map_err(failed);
            }
            Launch::Auto => {
                let dir = data_dir();
                let own = {
                    let mut child = self.child.lock().expect("child lock");
                    match child.as_mut() {
                        Some(server) => server
                            .alive()
                            .then(|| (server.url.clone(), server.token.clone())),
                        None => None,
                    }
                };
                if let Some((url, token)) = own {
                    match rpc::connect(&url, &token).await {
                        Ok(c) => return Ok(c),
                        Err(e) => {
                            tracing::info!(error = %e, "codex: own app-server refused; starting a new one");
                        }
                    }
                }
                let bin = match detect_tool(KIND).await.status {
                    ToolStatus::Ready { path, .. } => path,
                    ToolStatus::NotInstalled => {
                        return Err(RunnerError::NotInstalled(KIND.display_name()))
                    }
                    ToolStatus::TooOld { version, .. } => {
                        return Err(RunnerError::Failed(format!(
                            "Codex {version} is too old; the companion needs {} or newer",
                            KIND.min_version()
                        )))
                    }
                    ToolStatus::TooNew { version, .. } => {
                        return Err(RunnerError::Failed(format!(
                            "Codex {version} is newer than this companion supports; update the companion"
                        )))
                    }
                    ToolStatus::Unknown { .. } => {
                        return Err(RunnerError::Failed(
                            "could not read the Codex version".into(),
                        ))
                    }
                };
                let old = self.child.lock().expect("child lock").take();
                drop(old);
                server::stop_stale(&dir);
                let child = server::spawn(&bin, &dir, &crate::log::log_dir(&dir))
                    .await
                    .map_err(RunnerError::Failed)?;
                let target = (child.url.clone(), child.token.clone());
                *self.child.lock().expect("child lock") = Some(child);
                tracing::info!(url = %target.0, "codex: started app-server");
                target
            }
        };
        rpc::connect(&url, &token).await.map_err(failed)
    }

    fn on_closed(&self, generation: u64) {
        tracing::warn!("codex: connection closed");
        let mut ended = Vec::new();
        {
            let mut st = self.st();
            for (id, live) in st.threads.iter_mut() {
                if live.loaded == generation {
                    live.loaded = 0;
                    if live.turn.take().is_some() {
                        ended.push(id.clone());
                    }
                }
            }
            let gone: Vec<String> = st
                .prompts
                .iter()
                .filter(|(_, p)| p.generation == generation)
                .map(|(k, _)| k.clone())
                .collect();
            for k in gone {
                settle(&mut st, &k);
            }
        }
        for session_id in ended {
            self.emit(RunnerEvent::TurnEnded {
                tool: KIND,
                session_id,
                text: Some(
                    "Codex stopped before the turn finished (its app-server closed).".into(),
                ),
            });
        }
        self.emit(RunnerEvent::RosterChanged { tool: KIND });
    }

    fn on_notification(&self, generation: u64, method: &str, p: &Value) {
        let thread_id = p
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_string);
        match method {
            "turn/started" => {
                if let (Some(t), Some(turn)) =
                    (thread_id, p.pointer("/turn/id").and_then(Value::as_str))
                {
                    let mut st = self.st();
                    let live = st.threads.entry(t).or_default();
                    // A thread this server reports on is loaded here.
                    live.loaded = generation;
                    live.turn = Some(turn.to_string());
                    live.last_message = None;
                }
            }
            "turn/completed" => {
                let Some(t) = thread_id else { return };
                let turn = p.get("turn").cloned().unwrap_or(Value::Null);
                let text = {
                    let mut st = self.st();
                    let live = st.threads.entry(t.clone()).or_default();
                    live.turn = None;
                    live.done_turn = p
                        .pointer("/turn/id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    live.changes.clear();
                    let last = live.last_message.take();
                    let stale: Vec<String> = st
                        .prompts
                        .iter()
                        .filter(|(_, pr)| pr.session_id == t)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for k in stale {
                        settle(&mut st, &k);
                    }
                    map::turn_end_text(&turn, last)
                };
                self.emit(RunnerEvent::TurnEnded {
                    tool: KIND,
                    session_id: t,
                    text,
                });
            }
            "item/started" => {
                let (Some(t), Some(item)) = (thread_id, p.get("item")) else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("fileChange") {
                    if let Some(id) = item.get("id").and_then(Value::as_str) {
                        let paths = map::change_paths(item);
                        self.st()
                            .threads
                            .entry(t)
                            .or_default()
                            .changes
                            .insert(id.to_string(), paths);
                    }
                }
            }
            "item/completed" => {
                let (Some(t), Some(item)) = (thread_id, p.get("item")) else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        self.st().threads.entry(t.clone()).or_default().last_message =
                            Some(text.to_string());
                    }
                }
                if let Some(text) = map::item_progress(item) {
                    self.emit(RunnerEvent::Progress {
                        tool: KIND,
                        session_id: t,
                        text,
                    });
                }
            }
            "thread/status/changed" => {
                let Some(t) = thread_id else { return };
                let kind = p
                    .pointer("/status/type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let flags = p
                    .pointer("/status/activeFlags")
                    .and_then(Value::as_array)
                    .is_some_and(|f| !f.is_empty());
                let word = format!("{kind}{}", if flags { "+waiting" } else { "" });
                let changed = {
                    let mut st = self.st();
                    let live = st.threads.entry(t).or_default();
                    let changed = live.status.as_deref() != Some(word.as_str());
                    live.status = Some(word);
                    changed
                };
                if changed {
                    self.emit(RunnerEvent::RosterChanged { tool: KIND });
                }
            }
            "thread/closed" => {
                if let Some(t) = thread_id {
                    if let Some(live) = self.st().threads.get_mut(&t) {
                        live.loaded = 0;
                    }
                }
            }
            "thread/archived" => {
                if let Some(t) = thread_id {
                    let mut st = self.st();
                    st.threads.remove(&t);
                    st.touched.remove(&t);
                    // A closed unsent thread was never on Keo's roster.
                    let unsent = st.unsent.remove(&t).is_some();
                    drop(st);
                    if !unsent {
                        self.emit(RunnerEvent::SessionEnded {
                            tool: KIND,
                            session_id: t,
                            reason: "archived".into(),
                        });
                    }
                }
                self.emit(RunnerEvent::RosterChanged { tool: KIND });
            }
            "thread/started" | "thread/name/updated" | "thread/unarchived" => {
                self.emit(RunnerEvent::RosterChanged { tool: KIND });
            }
            "serverRequest/resolved" => {
                let Some(rid) = p.get("requestId") else {
                    return;
                };
                let mut st = self.st();
                let done: Vec<String> = st
                    .prompts
                    .iter()
                    .filter(|(_, pr)| &pr.request_id == rid)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in done {
                    settle(&mut st, &k);
                }
            }
            "account/rateLimits/updated" => {
                if let Some(r) = p.get("rateLimits") {
                    self.usage(r);
                }
            }
            "error" => {
                let message = p
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                tracing::warn!(message, "codex: turn error");
            }
            _ => {}
        }
    }

    /// Relay each plan window when it changed.
    fn usage(&self, rate_limits: &Value) {
        for (window, percent, resets_at) in map::usage_windows(rate_limits) {
            let fresh = {
                let mut st = self.st();
                let now = (percent, resets_at.clone());
                if st.usage.get(&window) == Some(&now) {
                    false
                } else {
                    st.usage.insert(window.clone(), now);
                    true
                }
            };
            if fresh {
                self.emit(RunnerEvent::Usage {
                    tool: KIND,
                    window,
                    percent,
                    resets_at,
                });
            }
        }
    }

    fn on_request(
        self: &Arc<Self>,
        generation: u64,
        conn: &Arc<Conn>,
        id: Value,
        method: &str,
        p: &Value,
    ) {
        let session_id = p
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let files = {
            let st = self.st();
            p.get("itemId")
                .and_then(Value::as_str)
                .and_then(|item| st.threads.get(&session_id)?.changes.get(item).cloned())
                .unwrap_or_default()
        };
        let Some(built) = map::questions(method, p, &files) else {
            tracing::info!(method, "codex: server request not handled");
            let _ = conn.respond_error(&id, -32601, "not supported by Keo Companion");
            return;
        };
        let group_key = (built.len() > 1 || method == "item/tool/requestUserInput")
            .then(|| format!("{generation}:{id}"));
        let mut asked = Vec::new();
        {
            let mut st = self.st();
            if let Some(g) = &group_key {
                st.groups.insert(
                    g.clone(),
                    Group {
                        answers: Map::new(),
                        open: HashSet::new(),
                    },
                );
            }
            for b in built {
                let prompt_id = uuid::Uuid::new_v4().to_string();
                if let Some(g) = &group_key {
                    if let Some(group) = st.groups.get_mut(g) {
                        group.open.insert(prompt_id.clone());
                    }
                }
                st.prompts.insert(
                    prompt_id.clone(),
                    Prompt {
                        generation,
                        request_id: id.clone(),
                        session_id: session_id.clone(),
                        ask: b.ask,
                        group: group_key.clone(),
                    },
                );
                asked.push((prompt_id, b.text, b.options));
            }
        }
        for (prompt_id, text, options) in asked {
            self.emit(RunnerEvent::Question {
                tool: KIND,
                session_id: session_id.clone(),
                prompt_id: prompt_id.clone(),
                text,
                options,
            });
            let me = Arc::downgrade(self);
            let wait = self.question_timeout;
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                let Some(me) = me.upgrade() else { return };
                if me.st().prompts.contains_key(&prompt_id) {
                    tracing::info!(prompt_id, "codex: question timed out; declining");
                    if let Err(e) = me.reply(&prompt_id, &Answer::Cancelled).await {
                        tracing::warn!(error = %e, "codex: timeout decline failed");
                    }
                }
            });
        }
    }

    /// Send the reply for one answered question.
    async fn reply(self: &Arc<Self>, prompt_id: &str, answer: &Answer) -> Result<(), RunnerError> {
        let (generation, request_id, out) = {
            let mut st = self.st();
            let Some(prompt) = st.prompts.remove(prompt_id) else {
                return if st.settled.iter().any(|s| s == prompt_id) {
                    Ok(())
                } else {
                    Err(RunnerError::NoSession(prompt_id.to_string()))
                };
            };
            let out = match map::reply(&prompt.ask, answer) {
                Reply::Done(v) => Some(v),
                Reply::Part {
                    question_id,
                    answers,
                } => {
                    let key = prompt.group.clone().unwrap_or_default();
                    let ready = match st.groups.get_mut(&key) {
                        Some(g) => {
                            g.answers.insert(question_id, json!({ "answers": answers }));
                            g.open.remove(prompt_id);
                            g.open.is_empty()
                        }
                        None => true,
                    };
                    if ready {
                        let answers = st
                            .groups
                            .remove(&key)
                            .map(|g| g.answers)
                            .unwrap_or_default();
                        Some(json!({ "answers": answers }))
                    } else {
                        None
                    }
                }
            };
            (prompt.generation, prompt.request_id, out)
        };
        let Some(result) = out else {
            remember(&mut self.st(), prompt_id);
            return Ok(());
        };
        let current = self.conn.lock().await.clone();
        let sent = match current {
            Some((g, conn)) if g == generation && !conn.is_closed() => {
                conn.respond(&request_id, result).map_err(failed)
            }
            _ => Err(RunnerError::Failed(
                "Codex restarted; that question is gone".into(),
            )),
        };
        // Only a reply that went out makes a repeat answer a no-op.
        if sent.is_ok() {
            remember(&mut self.st(), prompt_id);
        }
        sent
    }

    /// Load `session_id` on this connection (`thread/resume`) unless it already is.
    async fn ensure_loaded(self: &Arc<Self>, session_id: &str) -> Result<Arc<Conn>, RunnerError> {
        let (generation, conn) = self.conn().await?;
        let loaded = self
            .st()
            .threads
            .get(session_id)
            .is_some_and(|l| l.loaded == generation);
        if !loaded {
            let res = conn
                .call(
                    "thread/resume",
                    json!({"threadId": session_id, "excludeTurns": true}),
                    CALL_TIMEOUT,
                )
                .await
                .map_err(|e| match e {
                    RpcError::Server { .. } => RunnerError::NoSession(session_id.to_string()),
                    e => failed(e),
                })?;
            // `excludeTurns` leaves `thread.turns` empty: a thread resumed mid-turn (after a
            // companion restart, say) is found by its status, and its turn read from the newest page.
            let active =
                res.pointer("/thread/status/type").and_then(Value::as_str) == Some("active");
            let running = if active {
                match conn
                    .call(
                        "thread/turns/list",
                        json!({"threadId": session_id, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded"}),
                        CALL_TIMEOUT,
                    )
                    .await
                {
                    Ok(page) => page
                        .pointer("/data/0")
                        .filter(|t| t.get("status").and_then(Value::as_str) == Some("inProgress"))
                        .and_then(|t| t.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    Err(e) => {
                        tracing::warn!(error = %e, "codex: running turn not read");
                        None
                    }
                }
            } else {
                None
            };
            let mut st = self.st();
            let live = st.threads.entry(session_id.to_string()).or_default();
            live.loaded = generation;
            if running.is_some() {
                live.turn = running;
            }
        }
        self.st().touched.insert(session_id.to_string());
        Ok(conn)
    }

    /// `thread/name/set`; a failed name is logged, not fatal.
    async fn set_name(&self, conn: &Conn, id: &str, name: &str) {
        if let Err(e) = conn
            .call(
                "thread/name/set",
                json!({"threadId": id, "name": name}),
                CALL_TIMEOUT,
            )
            .await
        {
            tracing::warn!(error = %e, "codex: thread name not set");
        }
    }

    async fn turn_start(
        &self,
        conn: &Conn,
        session_id: &str,
        text: &str,
    ) -> Result<(), RunnerError> {
        let res = conn
            .call(
                "turn/start",
                json!({"threadId": session_id, "input": text_input(text)}),
                CALL_TIMEOUT,
            )
            .await
            .map_err(failed)?;
        if let Some(turn) = res.pointer("/turn/id").and_then(Value::as_str) {
            let mut st = self.st();
            let live = st.threads.entry(session_id.to_string()).or_default();
            if live.done_turn.as_deref() != Some(turn) {
                live.turn = Some(turn.to_string());
            }
        }
        Ok(())
    }

    async fn steer_turn(
        &self,
        conn: &Conn,
        session_id: &str,
        text: &str,
        turn: String,
    ) -> Result<(), RunnerError> {
        let call = |expected: String| {
            conn.call(
                "turn/steer",
                json!({"threadId": session_id, "input": text_input(text), "expectedTurnId": expected}),
                CALL_TIMEOUT,
            )
        };
        match call(turn).await {
            Ok(_) => Ok(()),
            // "expected active turn id `x` but found `y`": steer the turn that runs now.
            Err(RpcError::Server { message, .. }) if message.contains("but found `") => {
                let found = message
                    .split("but found `")
                    .nth(1)
                    .and_then(|r| r.split('`').next())
                    .unwrap_or("")
                    .to_string();
                if let Some(live) = self.st().threads.get_mut(session_id) {
                    live.turn = Some(found.clone());
                }
                call(found).await.map(|_| ()).map_err(failed)
            }
            Err(e) => Err(failed(e)),
        }
    }
}

/// Handle everything one connection sends, then clean up after it closes. It holds the runner
/// weakly, so a dropped runner (and the app-server it started) is not kept alive by its socket.
async fn pump(
    inner: std::sync::Weak<Inner>,
    generation: u64,
    conn: Arc<Conn>,
    mut inbound: tokio::sync::mpsc::UnboundedReceiver<Inbound>,
) {
    while let Some(msg) = inbound.recv().await {
        let Some(me) = inner.upgrade() else { return };
        match msg {
            Inbound::Notification { method, params } => {
                me.on_notification(generation, &method, &params)
            }
            Inbound::Request { id, method, params } => {
                me.on_request(generation, &conn, id, &method, &params)
            }
        }
    }
    if let Some(me) = inner.upgrade() {
        me.on_closed(generation);
    }
}

/// Forget an open question (answered elsewhere, or its turn ended).
fn settle(st: &mut State, prompt_id: &str) {
    if let Some(p) = st.prompts.remove(prompt_id) {
        if let Some(g) = p.group {
            st.groups.remove(&g);
        }
        remember(st, prompt_id);
    }
}

fn remember(st: &mut State, prompt_id: &str) {
    st.settled.push(prompt_id.to_string());
    if st.settled.len() > SETTLED_KEEP {
        st.settled.remove(0);
    }
}

#[async_trait]
impl Runner for CodexRunner {
    fn kind(&self) -> ToolKind {
        KIND
    }

    async fn detect(&self) -> Detection {
        detect_tool(KIND).await
    }

    async fn list(&self) -> Result<Vec<RosterEntry>, RunnerError> {
        let (_, conn) = self.inner.conn().await?;
        let creating = self.inner.creating.read().await;
        let page = conn
            .call(
                "thread/list",
                json!({"limit": ROSTER_PAGE, "sortKey": "recency_at", "archived": false}),
                CALL_TIMEOUT,
            )
            .await
            .map_err(failed)?;
        let threads: Vec<Value> = page
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (touched, unsent): (HashSet<String>, HashSet<String>) = {
            let st = self.inner.st();
            (st.touched.clone(), st.unsent.keys().cloned().collect())
        };
        drop(creating);
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            - ROSTER_DAYS * 86_400;
        let mut out: Vec<RosterEntry> = threads
            .iter()
            .filter(|t| {
                map::thread_touched(t) >= since
                    || t.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| touched.contains(id))
            })
            .filter_map(map::roster_entry)
            .filter(|e| !unsent.contains(&e.session_id))
            .collect();
        let missing: Vec<String> = touched
            .into_iter()
            .filter(|id| !out.iter().any(|e| &e.session_id == id))
            .collect();
        for id in missing {
            match conn
                .call(
                    "thread/read",
                    json!({"threadId": id, "includeTurns": false}),
                    CALL_TIMEOUT,
                )
                .await
            {
                Ok(r) => {
                    if let Some(e) = r.get("thread").and_then(map::roster_entry) {
                        out.push(e);
                    }
                }
                // A busy or failing server may still hold the thread: keep the last roster.
                Err(RpcError::Server { code, .. })
                    if code == rpc::OVERLOADED || code == INTERNAL_ERROR =>
                {
                    return Err(RunnerError::Failed(format!(
                        "Codex could not read thread {id} (error {code}); keeping the last roster"
                    )));
                }
                // Codex refused the thread itself (gone or unknown): it leaves the roster.
                Err(RpcError::Server { .. }) => {
                    self.inner.st().touched.remove(&id);
                }
                Err(e) => return Err(failed(e)),
            }
        }
        Ok(out)
    }

    async fn start(&self, req: StartRequest) -> Result<String, RunnerError> {
        let (generation, conn) = self.inner.conn().await?;
        let cwd = req.cwd.to_string_lossy().to_string();
        let creating = self.inner.creating.write().await;
        let res = conn
            .call(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "approvalPolicy": "on-request",
                    "sandbox": "workspace-write",
                    "config": lean_config(),
                }),
                CALL_TIMEOUT,
            )
            .await
            .map_err(failed)?;
        let thread = res.get("thread").cloned().unwrap_or(Value::Null);
        let id = thread
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| RunnerError::Failed("thread/start gave no thread id".into()))?
            .to_string();
        let named = req.name.clone();
        {
            let mut st = self.inner.st();
            st.threads.entry(id.clone()).or_default().loaded = generation;
            st.unsent.insert(id.clone(), named.is_none());
        }
        drop(creating);
        let name = named.unwrap_or_else(|| map::start_name("", req.slack_thread.as_deref()));
        self.inner.set_name(&conn, &id, &name).await;
        Ok(id)
    }

    async fn send(&self, session_id: &str, text: &str) -> Result<(), RunnerError> {
        let conn = self.inner.ensure_loaded(session_id).await?;
        // A session `start` opened gets its first words: it joins the roster now.
        let first = {
            let mut st = self.inner.st();
            let first = st.unsent.remove(session_id);
            if first.is_some() {
                st.touched.insert(session_id.to_string());
            }
            first
        };
        if first == Some(true) {
            if let Some(key) = map::ticket_key(text) {
                self.inner.set_name(&conn, session_id, &key).await;
            }
        }
        let running = self
            .inner
            .st()
            .threads
            .get(session_id)
            .and_then(|l| l.turn.clone());
        if let Some(turn) = running {
            match self.inner.steer_turn(&conn, session_id, text, turn).await {
                // The turn ended meanwhile (an interrupt answers before its turn/completed).
                Err(RunnerError::Failed(m)) if m.contains("no active turn") => {
                    if let Some(live) = self.inner.st().threads.get_mut(session_id) {
                        live.turn = None;
                    }
                }
                other => return other,
            }
        }
        self.inner.turn_start(&conn, session_id, text).await
    }

    async fn steer(&self, session_id: &str, text: &str) -> Result<(), RunnerError> {
        let conn = self.inner.ensure_loaded(session_id).await?;
        let turn = self
            .inner
            .st()
            .threads
            .get(session_id)
            .and_then(|l| l.turn.clone())
            .ok_or_else(|| RunnerError::Failed("no running turn to steer".into()))?;
        self.inner.steer_turn(&conn, session_id, text, turn).await
    }

    async fn interrupt(&self, session_id: &str) -> Result<(), RunnerError> {
        let conn = self.inner.ensure_loaded(session_id).await?;
        let turn = self
            .inner
            .st()
            .threads
            .get(session_id)
            .and_then(|l| l.turn.clone())
            .ok_or_else(|| RunnerError::Failed("no running turn to stop".into()))?;
        conn.call(
            "turn/interrupt",
            json!({"threadId": session_id, "turnId": turn}),
            INTERRUPT_TIMEOUT,
        )
        .await
        .map(|_| ())
        .map_err(failed)
    }

    async fn close(&self, session_id: &str) -> Result<(), RunnerError> {
        if !self.inner.st().unsent.contains_key(session_id) {
            return Ok(());
        }
        let (_, conn) = self.inner.conn().await?;
        let res = conn
            .call(
                "thread/archive",
                json!({"threadId": session_id}),
                CALL_TIMEOUT,
            )
            .await
            .map(|_| ())
            .map_err(failed);
        let mut st = self.inner.st();
        st.unsent.remove(session_id);
        st.threads.remove(session_id);
        res
    }

    async fn answer(&self, prompt_id: &str, answer: Answer) -> Result<(), RunnerError> {
        self.inner.reply(prompt_id, &answer).await
    }

    fn events(&self) -> broadcast::Receiver<RunnerEvent> {
        self.inner.events.subscribe()
    }
}
