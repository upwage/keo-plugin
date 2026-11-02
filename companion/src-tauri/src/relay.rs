//! The relay: runner events go to Keo as `coding_session_event` calls, and tasks and answers
//! from Keo go to the right runner. It only speaks `RunnerEvent`; it never looks inside a runner.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::keo::contract::{EventKind, PromptAnswer, SessionEvent, Task};
use crate::keo::{KeoClient, KeoError};
use crate::registry::Registry;
use crate::runner::{Answer, Runner, RunnerError, RunnerEvent, StartRequest, ToolKind};
use crate::state::{Event, Machine, Store};

/// Tries for one relayed event (backoff 2 s up to 1 min).
const EVENT_SEND_TRIES: usize = 6;

/// What `deliver` did with a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// A new session opened with this id, idle: the inbox registers it, then `first_turn` sends
    /// the task's words.
    Started(String),
    /// The text went to an existing session.
    Delivered,
}

/// The Keo event for a runner event; `None` for events that only move local state.
pub fn to_keo_event(ev: &RunnerEvent, machine_id: &str) -> Option<SessionEvent> {
    let out = match ev {
        RunnerEvent::SessionStarted {
            tool,
            session_id,
            cwd,
            name,
        } => {
            let mut e = SessionEvent::session(
                session_id,
                EventKind::SessionStart,
                tool.wire_name(),
                machine_id,
            );
            e.cwd = Some(cwd.clone());
            e.name = Some(name.clone());
            e
        }
        RunnerEvent::Progress {
            tool,
            session_id,
            text,
        } => {
            let mut e = SessionEvent::session(
                session_id,
                EventKind::Progress,
                tool.wire_name(),
                machine_id,
            );
            e.text = Some(text.clone());
            e
        }
        RunnerEvent::Question {
            tool,
            session_id,
            prompt_id,
            text,
            options,
        } => {
            let mut e = SessionEvent::session(
                session_id,
                EventKind::Question,
                tool.wire_name(),
                machine_id,
            );
            e.prompt_id = Some(prompt_id.clone());
            e.text = Some(text.clone());
            e.options = Some(options.clone());
            e
        }
        RunnerEvent::TurnEnded {
            tool,
            session_id,
            text,
        } => {
            let mut e =
                SessionEvent::session(session_id, EventKind::Stop, tool.wire_name(), machine_id);
            e.text = text.clone();
            e
        }
        // Usage is per machine and tool: no session id.
        RunnerEvent::Usage {
            tool,
            window,
            percent,
            resets_at,
        } => SessionEvent::usage(
            machine_id,
            tool.wire_name(),
            window,
            *percent,
            resets_at.clone(),
        ),
        RunnerEvent::SessionEnded {
            tool,
            session_id,
            reason,
        } => {
            let mut e =
                SessionEvent::session(session_id, EventKind::End, tool.wire_name(), machine_id);
            e.reason = Some(reason.clone());
            e
        }
        RunnerEvent::RosterChanged { .. } => return None,
    };
    Some(out)
}

/// The session a runner event is about, if any.
fn session_of(ev: &RunnerEvent) -> Option<&str> {
    match ev {
        RunnerEvent::SessionStarted { session_id, .. }
        | RunnerEvent::Progress { session_id, .. }
        | RunnerEvent::Question { session_id, .. }
        | RunnerEvent::TurnEnded { session_id, .. }
        | RunnerEvent::SessionEnded { session_id, .. } => Some(session_id),
        RunnerEvent::Usage { .. } | RunnerEvent::RosterChanged { .. } => None,
    }
}

/// Sessions held back before Keo binds them, and events held per session (bounded).
const HELD_SESSIONS: usize = 32;
const HELD_EVENTS: usize = 64;

/// The sessions the relay may speak for. A session is relayed only once Keo handed it work
/// under the current sign-in (`bind`, from the inbox): events of a session nobody bound are held
/// until it is bound under that same sign-in, and dropped when the sign-in changes. So no event,
/// queued or new, of a session started under one sign-in ever reaches another account, unless
/// that account itself then messages the session, and then only what follows the message. The
/// machine-wide state is derived from the bound sessions, so one session ending never shows the
/// machine idle while another still runs.
#[derive(Default)]
pub struct Sessions {
    generation: u64,
    owner: HashMap<String, u64>,
    active: HashSet<String>,
    waiting: HashSet<String>,
    held: HashMap<String, Vec<RunnerEvent>>,
}

/// What a bind does with the events held for its session.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Backlog {
    Replay,
    Drop,
}

/// Keep `ev` in a bounded pre-bind backlog. A full backlog drops progress notes and usage first,
/// so a question, a turn's end or a session's end is never lost (losing one would leave the
/// session working for good).
fn hold(events: &mut Vec<RunnerEvent>, ev: &RunnerEvent) {
    let minor =
        |e: &RunnerEvent| matches!(e, RunnerEvent::Progress { .. } | RunnerEvent::Usage { .. });
    if events.len() >= HELD_EVENTS {
        if minor(ev) {
            return;
        }
        let at = events.iter().position(minor).unwrap_or(0);
        events.remove(at);
    }
    events.push(ev.clone());
}

/// The folder a session really runs in, as its tool reports it: a task's cwd is only Keo's
/// word.
fn session_folder(
    roster: &[crate::runner::RosterEntry],
    session: &str,
) -> Result<String, RunnerError> {
    roster
        .iter()
        .find(|e| e.session_id == session)
        .map(|e| e.cwd.clone())
        .ok_or_else(|| RunnerError::NoSession(session.to_string()))
}

/// Whether `cwd` is inside a folder approved now.
fn approved_folder(approved: &[PathBuf], cwd: &str) -> Result<(), RunnerError> {
    if crate::dirs::contains(approved, Path::new(cwd)) {
        Ok(())
    } else {
        Err(RunnerError::Failed(
            "folder not approved on this machine".into(),
        ))
    }
}

/// What a bind replaced, for `unbind`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prior {
    owner: Option<u64>,
    active: bool,
}

/// What the relay does with one runner event.
#[derive(Debug, PartialEq)]
pub enum Seen {
    /// The session belongs to an earlier sign-in: drop the event, send nothing.
    Stale,
    /// Not bound yet: kept until Keo binds the session under this sign-in.
    Held,
    /// Relay it, and apply this machine-wide state change, if any.
    Relay(Option<Event>),
}

impl Sessions {
    /// A new sign-in starts with no running or waiting session and nothing held.
    fn roll(&mut self, generation: u64) {
        if generation != self.generation {
            self.generation = generation;
            self.active.clear();
            self.waiting.clear();
            self.held.clear();
        }
    }

    /// Keo handed work to session `id` under sign-in `generation`. Returns the events to relay
    /// now: with `Backlog::Replay` (a session Keo just started) the events held for it, which
    /// raced the start, or `None` when it belongs to an earlier sign-in. With `Backlog::Drop`
    /// (Keo messaged or answered a session that already existed) the session moves to this
    /// sign-in and what it said before is discarded: that was an earlier, unrelated turn.
    ///
    /// The session counts as running work from the bind on, before its first event, so another
    /// session ending meanwhile never shows the machine idle. The returned `Prior` lets a
    /// delivery that then fails undo the bind (`unbind`).
    pub fn bind(
        &mut self,
        id: &str,
        generation: u64,
        backlog: Backlog,
    ) -> Option<(Vec<RunnerEvent>, Prior)> {
        self.roll(generation);
        if backlog == Backlog::Replay && self.owner.get(id).is_some_and(|g| *g != generation) {
            return None;
        }
        let held = self.held.remove(id).unwrap_or_default();
        let prior = Prior {
            owner: self.owner.insert(id.to_string(), generation),
            active: !self.active.insert(id.to_string()),
        };
        let events = match backlog {
            Backlog::Replay => held,
            Backlog::Drop => Vec::new(),
        };
        Some((events, prior))
    }

    /// Undo a bind whose delivery failed: the session goes back to its earlier owner (or to no
    /// owner, so its own turns are held again) and stops counting as running work.
    pub fn unbind(&mut self, id: &str, generation: u64, prior: Prior) {
        if self.generation != generation || self.owner.get(id) != Some(&generation) {
            return;
        }
        match prior.owner {
            Some(g) => {
                self.owner.insert(id.to_string(), g);
            }
            None => {
                self.owner.remove(id);
            }
        }
        if !prior.active {
            self.active.remove(id);
            self.waiting.remove(id);
        }
    }

    /// Record `ev`, seen under sign-in `generation`.
    pub fn observe(&mut self, ev: &RunnerEvent, generation: u64) -> Seen {
        self.roll(generation);
        let Some(id) = session_of(ev) else {
            return Seen::Relay(None);
        };
        match self.owner.get(id).copied() {
            None => {
                if self.held.len() < HELD_SESSIONS || self.held.contains_key(id) {
                    hold(self.held.entry(id.to_string()).or_default(), ev);
                }
                return Seen::Held;
            }
            Some(owner) if owner != generation => {
                if matches!(ev, RunnerEvent::SessionEnded { .. }) {
                    self.owner.remove(id);
                }
                return Seen::Stale;
            }
            Some(_) => {}
        }
        let was_waiting = !self.waiting.is_empty();
        let was_idle = self.active.is_empty();
        match ev {
            RunnerEvent::SessionStarted { .. } | RunnerEvent::Progress { .. } => {
                self.active.insert(id.to_string());
                self.waiting.remove(id);
                // A bound session that finished Keo's turn and now runs a new one moves the
                // machine back to working.
                if was_idle {
                    return Seen::Relay(Some(Event::TaskStarted));
                }
            }
            RunnerEvent::Question { .. } => {
                self.active.insert(id.to_string());
                self.waiting.insert(id.to_string());
                return Seen::Relay(Some(Event::NeedsInput));
            }
            RunnerEvent::TurnEnded { .. } => {
                self.active.remove(id);
                self.waiting.remove(id);
            }
            RunnerEvent::SessionEnded { .. } => {
                self.owner.remove(id);
                self.active.remove(id);
                self.waiting.remove(id);
            }
            RunnerEvent::Usage { .. } | RunnerEvent::RosterChanged { .. } => {}
        }
        let ended = matches!(
            ev,
            RunnerEvent::TurnEnded { .. } | RunnerEvent::SessionEnded { .. }
        );
        Seen::Relay(if ended && self.active.is_empty() {
            Some(Event::TaskDone)
        } else if was_waiting && self.waiting.is_empty() {
            Some(Event::InputGiven)
        } else {
            None
        })
    }

    /// No bound session is running a turn.
    pub fn idle(&self) -> bool {
        self.active.is_empty()
    }

    /// Whether session `id` is bound to sign-in `generation`.
    pub fn bound(&mut self, id: &str, generation: u64) -> bool {
        self.roll(generation);
        self.owner.get(id) == Some(&generation)
    }

    /// A person's answer reached session `id`: InputGiven only once no question is left open.
    pub fn answered(&mut self, id: &str, generation: u64) -> Option<Event> {
        self.roll(generation);
        let was_waiting = !self.waiting.is_empty();
        self.waiting.remove(id);
        (was_waiting && self.waiting.is_empty()).then_some(Event::InputGiven)
    }
}

/// The runner's form of a person's answer: a chosen option wins over free text.
pub fn to_answer(a: &PromptAnswer) -> Answer {
    if a.cancelled {
        return Answer::Cancelled;
    }
    // Keo sends a chosen option as its index plus its text in `answer`; the runner takes the
    // option's text.
    match (a.option, &a.answer) {
        (Some(_), Some(t)) => Answer::Option(t.clone()),
        (Some(i), None) => Answer::Option(i.to_string()),
        (None, Some(t)) => Answer::Text(t.clone()),
        (None, None) => Answer::Cancelled,
    }
}

pub struct Relay {
    keo: Arc<KeoClient>,
    store: Arc<Store>,
    machine: Arc<Machine>,
    runners: Vec<Arc<dyn Runner>>,
    registry: Arc<Registry>,
    /// Which tool and session asked each open question, so its answer goes back there.
    prompts: Mutex<HashMap<String, (ToolKind, String)>>,
    sessions: Mutex<Sessions>,
    /// One ordered lane per session: the runner listener only queues an event, and the lane's
    /// task handles them one at a time, so a slow send (retries, backoff) delays only that
    /// session and the listener never lags behind the runner.
    lanes: Mutex<HashMap<String, mpsc::UnboundedSender<RunnerEvent>>>,
    /// Per session, held while its lane handles one event and while a bind replays its backlog:
    /// a live event never overtakes the held ones it follows.
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Relay {
    pub fn new(
        keo: Arc<KeoClient>,
        store: Arc<Store>,
        machine: Arc<Machine>,
        runners: Vec<Arc<dyn Runner>>,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            keo,
            store,
            machine,
            runners,
            registry,
            prompts: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Sessions::default()),
            lanes: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
        }
    }

    pub fn runner_for(&self, tool: ToolKind) -> Option<Arc<dyn Runner>> {
        self.runners.iter().find(|r| r.kind() == tool).cloned()
    }

    /// Refuses once the sign-in that pulled the work has ended: `current` is the inbox's own
    /// check (its ledger resets first at sign out) and `sign_in` the generation at the pull.
    fn still_current(
        &self,
        sign_in: u64,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RunnerError> {
        if current() && self.keo.sign_in_generation() == sign_in {
            Ok(())
        } else {
            Err(RunnerError::Failed("signed out meanwhile".into()))
        }
    }

    /// `still_current`, plus the folder still approved: checked right before every runner
    /// call, so a folder removed from the panel meanwhile stops the task there.
    fn admitted(
        &self,
        sign_in: u64,
        current: &(dyn Fn() -> bool + Sync),
        cwd: &str,
    ) -> Result<(), RunnerError> {
        self.still_current(sign_in, current)?;
        approved_folder(&self.store.get().approved_dirs, cwd)
    }

    /// Hand a task to its runner: open a new session (its words wait for `first_turn`), or send a
    /// message to an existing one. Checked
    /// against the pull's sign-in right before each runner call, so a task pulled for one
    /// account never runs after its sign-out.
    pub async fn deliver(
        &self,
        task: &Task,
        sign_in: u64,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<Delivery, RunnerError> {
        let tool = ToolKind::from_wire(&task.tool)
            .ok_or_else(|| RunnerError::Failed("unknown tool".into()))?;
        let runner = self
            .runner_for(tool)
            .ok_or(RunnerError::NotInstalled(tool.display_name()))?;
        if task.new {
            self.admitted(sign_in, current, &task.cwd)?;
            let id = runner
                .start(StartRequest {
                    cwd: PathBuf::from(&task.cwd),
                    name: None,
                    slack_thread: task
                        .slack_thread
                        .as_ref()
                        .map(|t| format!("{}/{}", t.channel, t.thread_ts)),
                })
                .await?;
            Ok(Delivery::Started(id))
        } else {
            let session = task
                .session_id
                .as_deref()
                .ok_or_else(|| RunnerError::Failed("task names no session".into()))?;
            // The folder boundary holds on the session's real folder, read from the tool.
            let cwd = session_folder(&runner.list().await?, session)?;
            self.admitted(sign_in, current, &cwd)?;
            // Bound before the send, so what the session says in reply is relayed; a failed send
            // undoes the bind, so a later local turn is not relayed as Keo's.
            let prior = self.bind(session, sign_in, Backlog::Drop).await?;
            let sent = match self.admitted(sign_in, current, &cwd) {
                Ok(()) => runner.send(session, &task.text).await,
                Err(e) => Err(e),
            };
            if let Err(e) = sent {
                self.unbind(session, sign_in, prior);
                return Err(e);
            }
            Ok(Delivery::Delivered)
        }
    }

    /// Send a new session's first words, once the inbox's Register tied it to `task`: bound
    /// first (replaying anything it said meanwhile), so its reply is relayed; a failed send undoes
    /// the bind. Checked against the pull's sign-in and the folder right before the send.
    pub async fn first_turn(
        &self,
        task: &Task,
        session: &str,
        sign_in: u64,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RunnerError> {
        let tool = ToolKind::from_wire(&task.tool)
            .ok_or_else(|| RunnerError::Failed("unknown tool".into()))?;
        let runner = self
            .runner_for(tool)
            .ok_or(RunnerError::NotInstalled(tool.display_name()))?;
        self.admitted(sign_in, current, &task.cwd)?;
        let prior = self.bind(session, sign_in, Backlog::Replay).await?;
        let sent = match self.admitted(sign_in, current, &task.cwd) {
            Ok(()) => runner.send(session, &task.text).await,
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            self.unbind(session, sign_in, prior);
            return Err(e);
        }
        Ok(())
    }

    /// No bound session is running a turn.
    pub fn idle(&self) -> bool {
        self.sessions.lock().expect("sessions lock").idle()
    }

    /// Bind a session to this sign-in and relay what `backlog` keeps of what it said before.
    async fn bind(
        &self,
        id: &str,
        generation: u64,
        backlog: Backlog,
    ) -> Result<Prior, RunnerError> {
        let gate = self.gate(id);
        let _gate = gate.lock().await;
        let (held, prior) = self
            .sessions
            .lock()
            .expect("sessions lock")
            .bind(id, generation, backlog)
            .ok_or_else(|| RunnerError::Failed("session belongs to an earlier sign-in".into()))?;
        // Acting before any of the session's events is relayed, so a turn that ends at once
        // (even before the ack) returns the machine to Listening instead of being lost.
        self.machine.apply(Event::TaskStarted);
        for ev in held {
            self.handle(ev).await;
        }
        Ok(prior)
    }

    /// Undo a bind whose delivery failed.
    fn unbind(&self, id: &str, generation: u64, prior: Prior) {
        self.sessions
            .lock()
            .expect("sessions lock")
            .unbind(id, generation, prior);
    }

    /// Hand an answer to the runner that asked the question; when the asker is not known
    /// (for example after a restart), try each runner until one owns the prompt. Returns the
    /// machine-wide state change: InputGiven only once no question is left open.
    pub async fn deliver_answer(
        &self,
        a: &PromptAnswer,
        sign_in: u64,
        current: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<Event>, RunnerError> {
        let answer = to_answer(a);
        let known = self
            .prompts
            .lock()
            .expect("prompts lock")
            .get(&a.prompt_id)
            .cloned();
        let session = a
            .session_id
            .clone()
            .or_else(|| known.as_ref().map(|(_, s)| s.clone()));
        let candidates: Vec<Arc<dyn Runner>> = match known.and_then(|(t, _)| self.runner_for(t)) {
            Some(r) => vec![r],
            None => self.runners.clone(),
        };
        // An answer resumes work in its session's folder: find the session's tool and real
        // folder, and check that folder is approved, before anything makes the session
        // relayable.
        let s = session
            .as_deref()
            .ok_or_else(|| RunnerError::NoSession(a.prompt_id.clone()))?;
        let mut found = None;
        let mut last = RunnerError::NoSession(a.prompt_id.clone());
        for r in candidates {
            match r.list().await.and_then(|roster| session_folder(&roster, s)) {
                Ok(cwd) => {
                    found = Some((r, cwd));
                    break;
                }
                Err(e @ (RunnerError::Unsupported(_) | RunnerError::NoSession(_))) => last = e,
                Err(e) => return Err(e),
            }
        }
        let (runner, cwd) = found.ok_or(last)?;
        self.admitted(sign_in, current, &cwd)?;
        // After a restart nothing is bound: Keo asked this session's question, so bind it
        // before the answer goes in, or what the session says next would be held for good.
        let bound = self
            .sessions
            .lock()
            .expect("sessions lock")
            .bound(s, sign_in);
        let reserved = if bound {
            None
        } else {
            Some(self.bind(s, sign_in, Backlog::Drop).await?)
        };
        // Checked again right before the answer: the bind's replay awaits.
        let outcome = match self.admitted(sign_in, current, &cwd) {
            Ok(()) => runner.answer(&a.prompt_id, answer).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                self.prompts
                    .lock()
                    .expect("prompts lock")
                    .remove(&a.prompt_id);
                Ok(session.and_then(|s| {
                    self.sessions
                        .lock()
                        .expect("sessions lock")
                        .answered(&s, sign_in)
                }))
            }
            Err(e) => {
                if let (Some(s), Some(prior)) = (&session, reserved) {
                    self.unbind(s, sign_in, prior);
                    if self.idle() {
                        self.machine.apply(Event::TaskDone);
                    }
                }
                Err(e)
            }
        }
    }

    /// One listener task per runner.
    pub fn spawn_listeners(self: &Arc<Self>) {
        for runner in &self.runners {
            let mut rx = runner.events();
            let me = Arc::clone(self);
            let tool = runner.kind();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => me.dispatch(ev),
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(tool = tool.wire_name(), skipped = n, "relay lagged");
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });
        }
    }

    fn gate(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.gates
                .lock()
                .expect("gates lock")
                .entry(id.to_string())
                .or_default(),
        )
    }

    /// Queue `ev` on its session's lane (started on first use). An event with no session
    /// (usage, roster) is handled on its own task: its order does not matter.
    fn dispatch(self: &Arc<Self>, ev: RunnerEvent) {
        let Some(id) = session_of(&ev).map(str::to_string) else {
            let me = Arc::clone(self);
            tauri::async_runtime::spawn(async move { me.handle(ev).await });
            return;
        };
        let mut lanes = self.lanes.lock().expect("lanes lock");
        let tx = lanes.entry(id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let me = Arc::clone(self);
            tauri::async_runtime::spawn(me.run_lane(id, rx));
            tx
        });
        let _ = tx.send(ev);
    }

    /// Handle one session's events in order. After its SessionEnded the lane closes: what is
    /// already queued is still handled, and a later event opens a new lane.
    async fn run_lane(self: Arc<Self>, id: String, mut rx: mpsc::UnboundedReceiver<RunnerEvent>) {
        let gate = self.gate(&id);
        while let Some(ev) = rx.recv().await {
            let ended = matches!(ev, RunnerEvent::SessionEnded { .. });
            if ended {
                self.lanes.lock().expect("lanes lock").remove(&id);
            }
            {
                let _gate = gate.lock().await;
                self.handle(ev).await;
            }
            if ended {
                rx.close();
            }
        }
        if !self.lanes.lock().expect("lanes lock").contains_key(&id) {
            self.gates.lock().expect("gates lock").remove(&id);
        }
    }

    async fn handle(&self, ev: RunnerEvent) {
        // Nothing is relayed while signed out (reading the machine id would recreate the
        // state.json sign-out just wiped), and a session from an earlier sign-in never reaches
        // the account signed in now.
        if !self.machine.snapshot().signed_in() {
            return;
        }
        let generation = self.keo.sign_in_generation();
        let seen = self
            .sessions
            .lock()
            .expect("sessions lock")
            .observe(&ev, generation);
        match seen {
            Seen::Stale => {
                tracing::info!("relay: event from an earlier sign-in dropped");
                return;
            }
            Seen::Held => return,
            Seen::Relay(Some(e)) => {
                self.machine.apply(e);
            }
            Seen::Relay(None) => {}
        }
        if let RunnerEvent::Question {
            tool,
            session_id,
            prompt_id,
            ..
        } = &ev
        {
            self.prompts
                .lock()
                .expect("prompts lock")
                .insert(prompt_id.clone(), (*tool, session_id.clone()));
        }
        if let RunnerEvent::RosterChanged { .. } = ev {
            if let Err(e) = self.registry.sync(generation).await {
                self.on_error(e);
            }
            return;
        }
        let Ok(Some(machine_id)) = self
            .store
            .machine_id_if(|| self.keo.sign_in_generation() == generation)
        else {
            return;
        };
        if let Some(out) = to_keo_event(&ev, &machine_id) {
            self.send_with_retry(generation, &out).await;
        }
    }

    /// Send one event, retrying a transient failure: a lost question would leave its session
    /// waiting for an answer that never comes. Retried inline, so this runner's events stay in
    /// order; a sign-out meanwhile ends it: every try is sent under the sign-in the event was
    /// accepted under, so a later one answers `Superseded`.
    async fn send_with_retry(&self, generation: u64, out: &SessionEvent) {
        let mut backoff =
            crate::doorbell::Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        for attempt in 1..=EVENT_SEND_TRIES {
            match self.keo.send_event(generation, out).await {
                Ok(()) => return,
                Err(e) if attempt < EVENT_SEND_TRIES && e.transient() => {
                    tracing::info!(error = %e, attempt, "relay: event send failed; retrying");
                }
                Err(e) => return self.on_error(e),
            }
            let wait = backoff.next_delay(&mut rand::thread_rng());
            tokio::time::sleep(wait).await;
        }
    }

    fn on_error(&self, e: KeoError) {
        match e {
            KeoError::SignedOut => {
                self.machine.apply(Event::Unauthorized);
            }
            other => tracing::warn!(error = %other, "relay call failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, id: &str) -> RunnerEvent {
        let (tool, session_id) = (ToolKind::Codex, id.to_string());
        match kind {
            "start" => RunnerEvent::SessionStarted {
                tool,
                session_id,
                cwd: "/w".into(),
                name: "w".into(),
            },
            "progress" => RunnerEvent::Progress {
                tool,
                session_id,
                text: "x".into(),
            },
            "question" => RunnerEvent::Question {
                tool,
                session_id,
                prompt_id: "p".into(),
                text: "?".into(),
                options: vec![],
            },
            "stop" => RunnerEvent::TurnEnded {
                tool,
                session_id,
                text: None,
            },
            _ => RunnerEvent::SessionEnded {
                tool,
                session_id,
                reason: "done".into(),
            },
        }
    }

    #[test]
    fn only_network_server_and_rate_limit_failures_are_retried() {
        assert!(KeoError::Network("reset".into()).transient());
        assert!(KeoError::RateLimited.transient());
        assert!(KeoError::Status(503).transient());
        assert!(KeoError::Status(429).transient());
        assert!(KeoError::Tool(crate::keo::contract::ErrorCode::KeoUnavailable).transient());
        assert!(!KeoError::Status(400).transient());
        assert!(!KeoError::SignedOut.transient());
        assert!(!KeoError::Superseded.transient());
    }

    #[test]
    fn bound_names_only_the_current_sign_in() {
        let mut s = Sessions::default();
        assert!(!s.bound("a", 1));
        s.bind("a", 1, Backlog::Replay).unwrap();
        assert!(s.bound("a", 1));
        assert!(!s.bound("a", 2));
    }

    #[test]
    fn the_machine_is_idle_only_when_every_bound_session_is() {
        let mut s = Sessions::default();
        s.bind("a", 1, Backlog::Replay).unwrap();
        s.bind("b", 1, Backlog::Replay).unwrap();
        assert_eq!(s.observe(&ev("start", "a"), 1), Seen::Relay(None));
        assert_eq!(s.observe(&ev("start", "b"), 1), Seen::Relay(None));
        assert_eq!(
            s.observe(&ev("question", "a"), 1),
            Seen::Relay(Some(Event::NeedsInput))
        );
        assert_eq!(
            s.observe(&ev("question", "b"), 1),
            Seen::Relay(Some(Event::NeedsInput))
        );
        // Answering one of two open questions leaves the machine waiting.
        assert_eq!(s.answered("a", 1), None);
        assert_eq!(s.answered("b", 1), Some(Event::InputGiven));
        // a ends while b still runs: not idle yet.
        assert_eq!(s.observe(&ev("stop", "a"), 1), Seen::Relay(None));
        assert_eq!(
            s.observe(&ev("end", "b"), 1),
            Seen::Relay(Some(Event::TaskDone))
        );
    }

    #[test]
    fn only_sessions_bound_under_this_sign_in_are_relayed() {
        let mut s = Sessions::default();
        // Events before the bind are held, then handed over by the bind.
        assert_eq!(s.observe(&ev("start", "a"), 1), Seen::Held);
        assert_eq!(
            s.bind("a", 1, Backlog::Replay).unwrap().0,
            vec![ev("start", "a")]
        );
        // A session nobody bound, queued under sign-in 1, is never adopted by sign-in 2.
        assert_eq!(s.observe(&ev("progress", "x"), 1), Seen::Held);
        assert_eq!(s.bind("x", 2, Backlog::Replay).unwrap().0, vec![]);
        // After the switch, a's events are stale and a cannot be bound again.
        assert_eq!(s.observe(&ev("progress", "a"), 2), Seen::Stale);
        assert!(s.bind("a", 2, Backlog::Replay).is_none());
        // An old session still running does not keep the new sign-in busy: once the work bound
        // under sign-in 2 (x and c) ends, the machine is done.
        s.bind("c", 2, Backlog::Replay).unwrap();
        assert_eq!(s.observe(&ev("start", "c"), 2), Seen::Relay(None));
        assert_eq!(s.observe(&ev("end", "x"), 2), Seen::Relay(None));
        assert_eq!(
            s.observe(&ev("end", "c"), 2),
            Seen::Relay(Some(Event::TaskDone))
        );
    }

    #[test]
    fn messaging_an_existing_session_drops_its_earlier_turn_and_moves_it() {
        let mut s = Sessions::default();
        // A local turn nobody bound: its reply is never relayed when Keo later messages it.
        assert_eq!(s.observe(&ev("progress", "a"), 1), Seen::Held);
        assert_eq!(s.observe(&ev("stop", "a"), 1), Seen::Held);
        assert_eq!(s.bind("a", 1, Backlog::Drop).unwrap().0, vec![]);
        assert_eq!(s.observe(&ev("progress", "a"), 1), Seen::Relay(None));
        // After a new sign-in, a message from that account moves the session to it.
        assert_eq!(s.observe(&ev("progress", "a"), 2), Seen::Stale);
        assert_eq!(s.bind("a", 2, Backlog::Drop).unwrap().0, vec![]);
        assert!(s.bound("a", 2));
        assert_eq!(s.observe(&ev("progress", "a"), 2), Seen::Relay(None));
    }

    #[test]
    fn a_bound_session_keeps_the_machine_busy_before_its_first_event() {
        let mut s = Sessions::default();
        s.bind("a", 1, Backlog::Replay).unwrap();
        assert_eq!(s.observe(&ev("start", "a"), 1), Seen::Relay(None));
        s.bind("b", 1, Backlog::Drop).unwrap();
        // a ends before b says anything: b is still working.
        assert_eq!(s.observe(&ev("stop", "a"), 1), Seen::Relay(None));
        assert!(!s.idle());
    }

    #[test]
    fn an_existing_session_must_sit_in_an_approved_folder() {
        use crate::runner::{RosterEntry, SessionState};
        let entry = |id: &str, cwd: &str| RosterEntry {
            tool: ToolKind::Codex,
            session_id: id.into(),
            cwd: cwd.into(),
            name: "n".into(),
            state: SessionState::Idle,
            pid: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap().join("work");
        let app = work.join("app");
        let other = tmp.path().canonicalize().unwrap().join("other");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let roster = vec![
            entry("in", app.to_str().unwrap()),
            entry("out", other.to_str().unwrap()),
        ];
        let approved = vec![work];
        let folder_of = |id: &str| session_folder(&roster, id);
        assert!(approved_folder(&approved, &folder_of("in").unwrap()).is_ok());
        // The task may name an approved cwd; the session's real folder decides.
        assert!(matches!(
            approved_folder(&approved, &folder_of("out").unwrap()),
            Err(RunnerError::Failed(_))
        ));
        assert!(matches!(folder_of("gone"), Err(RunnerError::NoSession(_))));
        // A folder removed from the panel is refused at the next runner call.
        assert!(approved_folder(&[], &folder_of("in").unwrap()).is_err());
    }

    #[test]
    fn a_full_backlog_keeps_the_turn_end() {
        let mut s = Sessions::default();
        for _ in 0..HELD_EVENTS + 10 {
            s.observe(&ev("progress", "a"), 1);
        }
        s.observe(&ev("stop", "a"), 1);
        s.observe(&ev("progress", "a"), 1);
        let held = &s.held["a"];
        assert_eq!(held.len(), HELD_EVENTS);
        assert!(matches!(held.last(), Some(RunnerEvent::TurnEnded { .. })));
    }

    #[test]
    fn a_failed_delivery_undoes_its_bind() {
        let mut s = Sessions::default();
        let (_, prior) = s.bind("a", 1, Backlog::Drop).unwrap();
        s.unbind("a", 1, prior);
        assert!(!s.bound("a", 1));
        assert!(s.idle());
        // Its next local turn is held again, not relayed.
        assert_eq!(s.observe(&ev("progress", "a"), 1), Seen::Held);
        // A session already bound and running keeps both after a failed second message.
        s.bind("b", 1, Backlog::Replay).unwrap();
        let (_, prior) = s.bind("b", 1, Backlog::Drop).unwrap();
        s.unbind("b", 1, prior);
        assert!(s.bound("b", 1));
        assert!(!s.idle());
    }

    #[test]
    fn maps_runner_events() {
        let q = RunnerEvent::Question {
            tool: ToolKind::Codex,
            session_id: "sess1234".into(),
            prompt_id: "p1".into(),
            text: "Run tests?".into(),
            options: vec!["yes".into(), "no".into()],
        };
        let v = to_keo_event(&q, "m1234567").unwrap().to_args();
        assert_eq!(v["event"], "Question");
        assert_eq!(v["options"][1], "no");

        let u = RunnerEvent::Usage {
            tool: ToolKind::ClaudeCode,
            window: "5h".into(),
            percent: 81.5,
            resets_at: None,
        };
        let v = to_keo_event(&u, "m1234567").unwrap().to_args();
        assert!(v.get("session_id").is_none());
        assert_eq!(v["tool"], "claude-code");
        assert_eq!(v["percent"], 81.5);
        assert!(v.get("resets_at").is_none());

        let stop = RunnerEvent::TurnEnded {
            tool: ToolKind::Codex,
            session_id: "sess1234".into(),
            text: Some("done".into()),
        };
        assert_eq!(to_keo_event(&stop, "m").unwrap().to_args()["event"], "Stop");

        let roster = RunnerEvent::RosterChanged {
            tool: ToolKind::Codex,
        };
        assert!(to_keo_event(&roster, "m").is_none());
    }

    #[test]
    fn answers_map_to_runner_form() {
        let a = |answer: Option<&str>, option: Option<u32>, cancelled: bool| PromptAnswer {
            prompt_id: "p1".into(),
            session_id: None,
            answer: answer.map(str::to_string),
            option,
            cancelled,
        };
        assert_eq!(
            to_answer(&a(Some("yes"), Some(0), false)),
            Answer::Option("yes".into())
        );
        assert_eq!(
            to_answer(&a(Some("go"), None, false)),
            Answer::Text("go".into())
        );
        assert_eq!(to_answer(&a(Some("go"), Some(1), true)), Answer::Cancelled);
        assert_eq!(to_answer(&a(None, None, false)), Answer::Cancelled);
    }
}
