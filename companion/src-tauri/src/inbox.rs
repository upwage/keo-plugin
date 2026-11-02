//! The inbox: on a ring, pull from Keo (at most one task, plus any answers to questions),
//! check each item, hand it to the relay, and ack it by `task_id` or `prompt_id`.
//!
//! Idempotency: the ledger below makes a task or an answer run at most once and keeps its ack
//! until Keo confirms it. Its tasks are also kept in state.json, written before a runner is
//! called, so a task pulled again after the app restarted (Keo's pull lease, 5 minutes, brings an
//! unacked item back) replays its ack and never runs twice. Answers are kept in memory only: the
//! question they answer is gone with the process anyway.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::doorbell::Target;
use crate::keo::contract::{
    ack_args, ack_confirmed, parse_pull, pull_args, AckTarget, Outcome, PromptAnswer, Task,
    TOOL_INBOX_ACK, TOOL_INBOX_PULL,
};
use crate::keo::{KeoClient, KeoError};
use crate::registry::Registry;
use crate::relay::{Delivery, Relay};
use crate::runner::ToolKind;
use crate::state::{Event, KeptTask, Machine, Store};

const REMEMBER: usize = 256;
/// Tries for the Register that ties a started session to its task, before its first words go in
/// (backoff 2 s up to 15 s, well inside the 5-minute pull lease).
const REGISTER_STARTED_TRIES: usize = 5;
/// The wait after Keo answers `rate_limited`.
const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(60);

/// What the ledger knows about a pulled item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// Never handled: decide and act on it.
    New,
    /// From an older sign-in: do nothing, write nothing.
    Stale,
    /// Handled and its ack confirmed by Keo. Keo handing it out again means that ack did not
    /// settle it (a task ack with a lease that had run out answers `already: true` and changes
    /// nothing), so the same ack is sent again with the new lease. Never acted on twice.
    Done(Outcome, Option<String>),
    /// Handled (delivered, refused or failed) but its ack has not been confirmed: do not act
    /// again, replay this ack.
    AckPending(Outcome, Option<String>),
}

/// The item ledger, keyed by ack target (task id or prompt id). An item becomes `Done` only
/// after Keo confirms its ack; until then it is `AckPending` with the outcome to replay, so it
/// is never run twice and never left unacked. Both lists are bounded.
///
/// Task and prompt ids are per account, so the ledger belongs to one sign-in: `reset` (at sign
/// out and at every new sign-in) starts a new generation, and every read and write names the
/// generation it was started under. A handler or an ack from an older generation reads as
/// already handled and writes nothing, so it can never refill the ledger for the next account.
#[derive(Default)]
pub struct Ledger {
    generation: u64,
    done: VecDeque<(AckTarget, Outcome, Option<String>)>,
    pending: VecDeque<(AckTarget, Outcome, Option<String>)>,
}

impl Ledger {
    /// Forget every item and start a new generation.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.done.clear();
        self.pending.clear();
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn check(&self, generation: u64, id: &AckTarget) -> Seen {
        if generation != self.generation {
            return Seen::Stale;
        }
        if let Some((_, o, d)) = self.pending.iter().find(|(p, _, _)| p == id) {
            return Seen::AckPending(*o, d.clone());
        }
        if let Some((_, o, d)) = self.done.iter().find(|(p, _, _)| p == id) {
            return Seen::Done(*o, d.clone());
        }
        Seen::New
    }

    /// Record the outcome before the ack is sent (and before the runner is called).
    pub fn pending(
        &mut self,
        generation: u64,
        id: &AckTarget,
        outcome: Outcome,
        detail: Option<String>,
    ) {
        if generation != self.generation {
            return;
        }
        self.pending.retain(|(p, _, _)| p != id);
        if self.pending.len() == REMEMBER {
            self.pending.pop_front();
        }
        self.pending.push_back((id.clone(), outcome, detail));
    }

    /// Keo confirmed the ack.
    pub fn confirm(&mut self, generation: u64, id: &AckTarget) {
        if generation != self.generation {
            return;
        }
        let Some(at) = self.pending.iter().position(|(p, _, _)| p == id) else {
            return;
        };
        let entry = self.pending.remove(at).expect("position is in range");
        self.done.retain(|(p, _, _)| p != id);
        if self.done.len() == REMEMBER {
            self.done.pop_front();
        }
        self.done.push_back(entry);
    }

    /// Settle an ack reply: `Done` only on a confirmed reply; anything else stays pending.
    /// Returns whether it was confirmed.
    pub fn settle(
        &mut self,
        generation: u64,
        id: &AckTarget,
        reply: &Result<Value, KeoError>,
    ) -> bool {
        match reply {
            Ok(v) if generation == self.generation && ack_confirmed(v) => {
                self.confirm(generation, id);
                true
            }
            _ => false,
        }
    }
}

fn outcome_word(o: Outcome) -> &'static str {
    match o {
        Outcome::Delivered => "delivered",
        Outcome::Started => "started",
        Outcome::Failed => "failed",
    }
}

fn outcome_of(word: &str) -> Option<Outcome> {
    match word {
        "delivered" => Some(Outcome::Delivered),
        "started" => Some(Outcome::Started),
        "failed" => Some(Outcome::Failed),
        _ => None,
    }
}

impl Ledger {
    /// The tasks to keep in state.json: pending ones unconfirmed, done ones confirmed.
    pub fn kept(&self) -> Vec<KeptTask> {
        let rows = |list: &VecDeque<(AckTarget, Outcome, Option<String>)>, confirmed: bool| {
            list.iter()
                .filter_map(|(id, o, d)| match id {
                    AckTarget::Task(t) => Some(KeptTask {
                        task_id: *t,
                        outcome: outcome_word(*o).to_string(),
                        detail: d.clone(),
                        confirmed,
                    }),
                    AckTarget::Prompt(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let mut out = rows(&self.done, true);
        out.extend(rows(&self.pending, false));
        out
    }

    /// Take back the tasks kept in state.json, under the current generation.
    pub fn restore(&mut self, kept: &[KeptTask]) {
        let generation = self.generation;
        for k in kept {
            let Some(o) = outcome_of(&k.outcome) else {
                continue;
            };
            let id = AckTarget::Task(k.task_id);
            self.pending(generation, &id, o, k.detail.clone());
            if k.confirmed {
                self.confirm(generation, &id);
            }
        }
    }
}

/// Why a task is refused before it reaches a runner.
pub fn refusal(task: &Task, approved: &[std::path::PathBuf]) -> Option<&'static str> {
    if !task.new && task.session_id.is_none() {
        return Some("task names no session");
    }
    if !crate::dirs::contains(approved, Path::new(&task.cwd)) {
        return Some("folder not approved on this machine");
    }
    None
}

fn label(id: &AckTarget) -> String {
    match id {
        AckTarget::Task(t) => format!("task {t}"),
        AckTarget::Prompt(p) => format!("prompt {p}"),
    }
}

pub struct Inbox {
    keo: Arc<KeoClient>,
    store: Arc<Store>,
    machine: Arc<Machine>,
    relay: Arc<Relay>,
    registry: Arc<Registry>,
    ledger: Arc<Mutex<Ledger>>,
}

impl Inbox {
    pub fn new(
        keo: Arc<KeoClient>,
        store: Arc<Store>,
        machine: Arc<Machine>,
        relay: Arc<Relay>,
        registry: Arc<Registry>,
    ) -> Self {
        let mut ledger = Ledger::default();
        ledger.restore(&store.get().inbox_tasks);
        Self {
            keo,
            store,
            machine,
            relay,
            registry,
            ledger: Arc::new(Mutex::new(ledger)),
        }
    }

    /// At sign out and at every new sign-in: forget the ledger (the kept copy too), and turn
    /// every handler and ack still in flight into a no-op.
    pub fn forget(&self) {
        let mut ledger = self.ledger.lock().expect("ledger lock");
        ledger.reset();
        if let Err(e) = self.store.update(|p| p.inbox_tasks.clear()) {
            tracing::warn!(error = %e, "inbox: kept tasks not cleared");
        }
    }

    /// Write the ledger's tasks to state.json, unless sign-in `generation` has ended. Held
    /// under the ledger lock, so a later state is never overwritten by an earlier one.
    fn save(&self, generation: u64) -> std::io::Result<()> {
        let ledger = self.ledger.lock().expect("ledger lock");
        if ledger.generation() != generation {
            return Ok(());
        }
        let kept = ledger.kept();
        self.store.update(|p| p.inbox_tasks = kept)
    }

    fn generation(&self) -> u64 {
        self.ledger.lock().expect("ledger lock").generation()
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    pub async fn run(self: Arc<Self>, mut rings: mpsc::Receiver<Target>) {
        while let Some(target) = rings.recv().await {
            self.handle(target).await;
        }
    }

    async fn handle(&self, target: Target) {
        // A ring that waited in the queue while the switch was turned on is dropped here.
        if !self.machine.snapshot().accepts_work() {
            tracing::info!("inbox: ring ignored (paused or signed out)");
            return;
        }
        let generation = self.generation();
        let sign_in = self.keo.sign_in_generation();
        self.machine.apply(Event::Ring);
        let Ok(Some(machine_id)) = self
            .store
            .machine_id_if(|| self.keo.sign_in_generation() == sign_in)
        else {
            self.machine.apply(Event::NothingToDo);
            return;
        };
        let session = match &target {
            Target::Session(s) => Some(s.as_str()),
            _ => None,
        };
        let pulled = self
            .keo
            .call_tool(TOOL_INBOX_PULL, pull_args(session, &machine_id))
            .await;
        if !self.is_current(generation) {
            tracing::info!("inbox: pull dropped (signed out meanwhile)");
            return;
        }
        let reply = match pulled {
            Ok(v) => match parse_pull(v) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "inbox pull reply rejected");
                    self.machine.apply(Event::NothingToDo);
                    return;
                }
            },
            Err(KeoError::SignedOut) => {
                self.machine.apply(Event::Unauthorized);
                return;
            }
            Err(KeoError::Superseded) => return,
            Err(KeoError::RateLimited) => {
                tracing::info!("inbox pull rate limited; backing off");
                self.machine.apply(Event::NothingToDo);
                tokio::time::sleep(RATE_LIMIT_PAUSE).await;
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "inbox pull failed");
                self.machine.apply(Event::NothingToDo);
                return;
            }
        };
        for answer in &reply.answers {
            self.handle_answer(generation, sign_in, answer).await;
        }
        match reply.task {
            Some(task) => self.handle_task(generation, sign_in, &task).await,
            None => {
                tracing::info!(answers = reply.answers.len(), "inbox: no task");
                self.machine.apply(Event::NothingToDo);
            }
        }
    }

    /// Returns true when the item is new and should be acted on; replays a pending ack.
    async fn first_time(&self, generation: u64, id: &AckTarget, lease: Option<&str>) -> bool {
        let seen = self
            .ledger
            .lock()
            .expect("ledger lock")
            .check(generation, id);
        match seen {
            Seen::New => true,
            Seen::Stale => false,
            Seen::Done(outcome, detail) => {
                tracing::info!(item = %label(id), "inbox: handed out again; acking again");
                self.ack(generation, id, lease, outcome, detail).await;
                false
            }
            Seen::AckPending(outcome, detail) => {
                tracing::info!(item = %label(id), "inbox: replaying a pending ack");
                self.ack(generation, id, lease, outcome, detail).await;
                false
            }
        }
    }

    async fn handle_answer(&self, generation: u64, sign_in: u64, answer: &PromptAnswer) {
        let id = AckTarget::Prompt(answer.prompt_id.clone());
        if !self.first_time(generation, &id, None).await {
            return;
        }
        if !self.machine.snapshot().accepts_work() {
            self.ack(
                generation,
                &id,
                None,
                Outcome::Failed,
                Some("paused on this machine".into()),
            )
            .await;
            return;
        }
        self.ledger.lock().expect("ledger lock").pending(
            generation,
            &id,
            Outcome::Failed,
            Some("delivery interrupted".into()),
        );
        if !self.is_current(generation) {
            return;
        }
        // Pause is rechecked at each runner call too: answers stop when paused or signed out.
        let current = || self.is_current(generation) && self.machine.snapshot().accepts_work();
        match self.relay.deliver_answer(answer, sign_in, &current).await {
            Ok(change) => {
                self.ack(generation, &id, None, Outcome::Delivered, None)
                    .await;
                if let Some(e) = change {
                    self.machine.apply(e);
                }
            }
            Err(e) => {
                self.ack(generation, &id, None, Outcome::Failed, Some(e.to_string()))
                    .await
            }
        }
    }

    async fn handle_task(&self, generation: u64, sign_in: u64, task: &Task) {
        let id = AckTarget::Task(task.task_id);
        let lease = Some(task.lease.as_str());
        if !self.first_time(generation, &id, lease).await {
            self.machine.apply(Event::NothingToDo);
            return;
        }
        // Checked again after the pull: the switch may have been turned on, or a tool check may
        // have found a tool out of range, meanwhile.
        let snap = self.machine.snapshot();
        let refused = if snap.paused {
            Some("paused on this machine")
        } else if !snap.accepts_tasks() {
            Some("a coding tool on this machine needs an update")
        } else {
            refusal(task, &self.store.get().approved_dirs)
        };
        if let Some(why) = refused {
            tracing::info!(task_id = task.task_id, reason = why, "inbox: task refused");
            self.ack(
                generation,
                &id,
                lease,
                Outcome::Failed,
                Some(why.to_string()),
            )
            .await;
            self.machine.apply(Event::NothingToDo);
            return;
        }
        // Recorded, and written to state.json, before the runner is called: if the ack never
        // lands, a later pull of this task (after a restart too) replays an ack and never runs it
        // a second time. A task that cannot be recorded is not run.
        self.ledger.lock().expect("ledger lock").pending(
            generation,
            &id,
            Outcome::Failed,
            Some("delivery interrupted".into()),
        );
        if let Err(e) = self.save(generation) {
            tracing::warn!(task_id = task.task_id, error = %e, "inbox: task not recorded");
            self.ack(
                generation,
                &id,
                lease,
                Outcome::Failed,
                Some("could not record the task on this machine".into()),
            )
            .await;
            self.machine.apply(Event::NothingToDo);
            return;
        }
        if !self.is_current(generation) {
            return;
        }
        // Rechecked right before each runner call: Pause, a tool found out of range, or a
        // sign-out since the snapshot above stops the task there.
        let current = || self.is_current(generation) && self.machine.snapshot().accepts_tasks();
        match self.relay.deliver(task, sign_in, &current).await {
            Ok(Delivery::Started(session_id)) => {
                // The session is open and idle. Keo must hear of it from the Register that ties it
                // to this task first, then its words go in, then the ack.
                let started = match self.register_started(sign_in, &session_id, task).await {
                    Ok(()) => {
                        self.relay
                            .first_turn(task, &session_id, sign_in, &current)
                            .await
                    }
                    Err(why) => Err(crate::runner::RunnerError::Failed(why)),
                };
                match started {
                    Ok(()) => {
                        // The relay entered Acting when it bound the session, before any of its
                        // events.
                        tracing::info!(task_id = task.task_id, "inbox: session started");
                        self.ack(generation, &id, lease, Outcome::Started, None)
                            .await;
                    }
                    Err(_) if !self.is_current(generation) => {
                        tracing::info!(
                            task_id = task.task_id,
                            "inbox: task dropped (signed out meanwhile)"
                        );
                        self.close_started(task, &session_id).await;
                    }
                    Err(e) => {
                        tracing::info!(task_id = task.task_id, error = %e, "inbox: start failed");
                        self.close_started(task, &session_id).await;
                        self.ack(generation, &id, lease, Outcome::Failed, Some(e.to_string()))
                            .await;
                        self.machine.apply(Event::NothingToDo);
                        if self.relay.idle() {
                            self.machine.apply(Event::TaskDone);
                        }
                    }
                }
            }
            Ok(Delivery::Delivered) => {
                tracing::info!(task_id = task.task_id, "inbox: delivered");
                self.ack(generation, &id, lease, Outcome::Delivered, None)
                    .await;
            }
            Err(_) if !self.is_current(generation) => {
                tracing::info!(
                    task_id = task.task_id,
                    "inbox: task dropped (signed out meanwhile)"
                );
            }
            Err(e) => {
                tracing::info!(task_id = task.task_id, error = %e, "inbox: delivery failed");
                self.ack(generation, &id, lease, Outcome::Failed, Some(e.to_string()))
                    .await;
                // Still Pulling (nothing bound), or Acting from a bind whose send then failed.
                self.machine.apply(Event::NothingToDo);
                if self.relay.idle() {
                    self.machine.apply(Event::TaskDone);
                }
            }
        }
    }

    /// End a new session that never got its words in, so it does not sit idle until the app quits.
    async fn close_started(&self, task: &Task, session_id: &str) {
        let Some(runner) = ToolKind::from_wire(&task.tool).and_then(|t| self.relay.runner_for(t))
        else {
            return;
        };
        if let Err(e) = runner.close(session_id).await {
            tracing::debug!(error = %e, "inbox: unused session not closed");
        }
    }

    /// Tie a new session to its task's Slack thread, retrying a passing failure inline: its
    /// words wait for this. Stops on success, on a refusal that retrying cannot fix, or when the
    /// sign-in ends; the error says why.
    async fn register_started(
        &self,
        sign_in: u64,
        session_id: &str,
        task: &Task,
    ) -> Result<(), String> {
        let tool = ToolKind::from_wire(&task.tool).ok_or("unknown tool")?;
        let mut backoff =
            crate::doorbell::Backoff::new(Duration::from_secs(2), Duration::from_secs(15));
        for attempt in 1..=REGISTER_STARTED_TRIES {
            match self
                .registry
                .register_started(
                    sign_in,
                    tool,
                    session_id,
                    task.task_id,
                    &task.lease,
                    &task.cwd,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(KeoError::Superseded) => return Err("signed out meanwhile".into()),
                Err(e) if e.transient() && attempt < REGISTER_STARTED_TRIES => {
                    tracing::warn!(task_id = task.task_id, error = %e, "started Register failed; retrying");
                }
                Err(e) => {
                    tracing::warn!(task_id = task.task_id, error = %e, "started Register failed");
                    return Err(format!("the new session could not be registered: {e}"));
                }
            }
            let wait = backoff.next_delay(&mut rand::thread_rng());
            tokio::time::sleep(wait).await;
        }
        Err("the new session could not be registered".into())
    }

    /// Record the outcome as pending, send the ack, and mark the item done only when Keo
    /// confirms it (`ok: true`). Anything else stays pending and is replayed on the next pull.
    /// An ack from an older sign-in is never sent: the client now speaks for another account.
    async fn ack(
        &self,
        generation: u64,
        id: &AckTarget,
        lease: Option<&str>,
        outcome: Outcome,
        detail: Option<String>,
    ) {
        if !self.is_current(generation) {
            return;
        }
        self.ledger
            .lock()
            .expect("ledger lock")
            .pending(generation, id, outcome, detail.clone());
        let reply = self
            .keo
            .call_tool(
                TOOL_INBOX_ACK,
                ack_args(id, lease, outcome, detail.as_deref()),
            )
            .await;
        let confirmed = self
            .ledger
            .lock()
            .expect("ledger lock")
            .settle(generation, id, &reply);
        if matches!(id, AckTarget::Task(_)) {
            if let Err(e) = self.save(generation) {
                tracing::warn!(item = %label(id), error = %e, "inbox: ack not recorded");
            }
        }
        if !confirmed {
            match reply {
                Err(e) => {
                    tracing::warn!(item = %label(id), error = %e, "inbox ack failed; will replay")
                }
                Ok(_) => tracing::warn!(item = %label(id), "inbox ack not confirmed; will replay"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(id: i64) -> AckTarget {
        AckTarget::Task(id)
    }

    #[test]
    fn done_only_after_confirmed_ack() {
        let mut l = Ledger::default();
        assert_eq!(l.check(0, &t(1)), Seen::New);
        // Outcome recorded, ack not yet confirmed (or it failed): replay, never redo.
        l.pending(
            0,
            &t(1),
            Outcome::Failed,
            Some("folder not approved".into()),
        );
        assert_eq!(
            l.check(0, &t(1)),
            Seen::AckPending(Outcome::Failed, Some("folder not approved".into()))
        );
        l.confirm(0, &t(1));
        assert_eq!(
            l.check(0, &t(1)),
            Seen::Done(Outcome::Failed, Some("folder not approved".into()))
        );
        // Task 1 and prompt "1" are different items.
        assert_eq!(l.check(0, &AckTarget::Prompt("1".into())), Seen::New);
    }

    #[test]
    fn settle_requires_ok_true() {
        let mut l = Ledger::default();
        let id = AckTarget::Prompt("p1".into());
        l.pending(0, &id, Outcome::Delivered, None);
        for reply in [
            Ok(json!({})),
            Ok(Value::Null),
            Ok(json!({"ok": false})),
            Ok(json!({"prompt_id": "p1"})),
            Err(KeoError::Protocol("bad".into())),
        ] {
            assert!(!l.settle(0, &id, &reply));
            assert_eq!(l.check(0, &id), Seen::AckPending(Outcome::Delivered, None));
        }
        assert!(l.settle(0, &id, &Ok(json!({"ok": true, "already": true}))));
        assert_eq!(l.check(0, &id), Seen::Done(Outcome::Delivered, None));
    }

    #[test]
    fn reset_forgets_and_fences_off_the_old_sign_in() {
        let mut l = Ledger::default();
        let a = AckTarget::Task(1);
        let b = AckTarget::Task(2);
        l.pending(0, &a, Outcome::Delivered, None);
        l.pending(0, &b, Outcome::Failed, None);
        l.confirm(0, &b);
        assert!(matches!(l.check(0, &a), Seen::AckPending(..)));
        assert!(matches!(l.check(0, &b), Seen::Done(..)));
        l.reset();
        let now = l.generation();
        assert_ne!(now, 0);
        assert_eq!(l.check(now, &a), Seen::New);
        assert_eq!(l.check(now, &b), Seen::New);
        // An ack or handler from the old sign-in writes nothing and reads as handled.
        l.pending(0, &a, Outcome::Failed, Some("old".into()));
        assert!(!l.settle(0, &b, &Ok(json!({"ok": true, "already": false}))));
        l.confirm(0, &a);
        assert_eq!(l.check(now, &a), Seen::New);
        assert_eq!(l.check(now, &b), Seen::New);
        assert_eq!(l.check(0, &a), Seen::Stale);
    }

    #[test]
    fn kept_tasks_survive_a_restart() {
        let mut l = Ledger::default();
        l.pending(0, &t(1), Outcome::Started, None);
        l.confirm(0, &t(1));
        // Recorded before delivery, and the app died before Keo confirmed the ack.
        l.pending(
            0,
            &t(2),
            Outcome::Failed,
            Some("delivery interrupted".into()),
        );
        l.pending(0, &AckTarget::Prompt("p".into()), Outcome::Delivered, None);
        let kept = l.kept();
        assert_eq!(kept.len(), 2, "prompts are not kept");
        let json = serde_json::to_string(&kept).unwrap();

        let mut after = Ledger::default();
        after.reset();
        after.restore(&serde_json::from_str::<Vec<KeptTask>>(&json).unwrap());
        let now = after.generation();
        assert_eq!(after.check(now, &t(1)), Seen::Done(Outcome::Started, None));
        // Replayed as the failure it recorded, never run again.
        assert_eq!(
            after.check(now, &t(2)),
            Seen::AckPending(Outcome::Failed, Some("delivery interrupted".into()))
        );
        assert_eq!(after.check(now, &AckTarget::Prompt("p".into())), Seen::New);
        // An unknown outcome word is skipped.
        after.restore(&[KeptTask {
            task_id: 3,
            outcome: "maybe".into(),
            detail: None,
            confirmed: true,
        }]);
        assert_eq!(after.check(now, &t(3)), Seen::New);
    }

    #[test]
    fn ledger_is_bounded() {
        let mut l = Ledger::default();
        l.pending(0, &t(-1), Outcome::Started, None);
        l.confirm(0, &t(-1));
        for i in 0..REMEMBER as i64 {
            l.pending(0, &t(i), Outcome::Delivered, None);
            l.confirm(0, &t(i));
        }
        // The first one rolled off the end.
        assert_eq!(l.check(0, &t(-1)), Seen::New);
        for i in 0..(REMEMBER + 5) as i64 {
            l.pending(0, &t(10_000 + i), Outcome::Delivered, None);
        }
        assert_eq!(l.pending.len(), REMEMBER);
    }

    #[test]
    fn refuses_unapproved_folders_and_targetless_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ok = root.join("ok");
        let other = root.join("other");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mk = |cwd: &Path, new: bool| Task {
            task_id: 1,
            lease: "lease-1".into(),
            session_id: None,
            new,
            tool: "codex".into(),
            cwd: cwd.display().to_string(),
            text: String::new(),
            slack_thread: None,
            approved_at: None,
        };
        let approved = vec![ok.clone()];
        assert_eq!(refusal(&mk(&ok, true), &approved), None);
        assert!(refusal(&mk(&other, true), &approved).is_some());
        assert!(refusal(&mk(&ok, true), &[]).is_some());
        assert_eq!(
            refusal(&mk(&ok, false), &approved),
            Some("task names no session")
        );
    }
}
