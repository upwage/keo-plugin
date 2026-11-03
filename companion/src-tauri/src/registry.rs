//! The local roster: every session the runners list, sent to Keo as `Register` events. Each sync
//! sends only what changed since the last one (saved in state.json as `last_roster`). The
//! machine itself registers once, with no session id.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::keo::contract::{EventKind, SessionEvent};
use crate::keo::{KeoClient, KeoError};
use crate::runner::{Detection, RosterEntry, Runner, SessionState, ToolKind, ToolStatus};
use crate::state::Store;

/// Tries for one roster event (backoff 2 s up to 1 min).
const ROSTER_SEND_TRIES: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterChange {
    /// New, or its name, state, cwd or pid changed.
    Upsert(RosterEntry),
    Removed(RosterEntry),
}

/// Whether Keo's view of a session is unchanged: everything but `active_at`, which only orders
/// the panel and would otherwise send a Register on every turn.
fn same_for_keo(a: &RosterEntry, b: &RosterEntry) -> bool {
    a.tool == b.tool
        && a.session_id == b.session_id
        && a.cwd == b.cwd
        && a.name == b.name
        && a.state == b.state
        && a.pid == b.pid
}

/// What changed from `old` to `new`, keyed by (tool, session id).
pub fn diff(old: &[RosterEntry], new: &[RosterEntry]) -> Vec<RosterChange> {
    let key = |e: &RosterEntry| (e.tool, e.session_id.clone());
    let mut out = Vec::new();
    for n in new {
        match old.iter().find(|o| key(o) == key(n)) {
            Some(o) if same_for_keo(o, n) => {}
            _ => out.push(RosterChange::Upsert(n.clone())),
        }
    }
    for o in old {
        if !new.iter().any(|n| key(n) == key(o)) {
            out.push(RosterChange::Removed(o.clone()));
        }
    }
    out
}

/// `diff`, plus an Upsert for every entry of `new` that `diff` left out: a full re-send that
/// still removes what `old` had and `new` lacks.
pub fn resync(old: &[RosterEntry], new: &[RosterEntry]) -> Vec<RosterChange> {
    let mut out = diff(old, new);
    for n in new {
        let listed = out
            .iter()
            .any(|c| matches!(c, RosterChange::Upsert(e) if e.tool == n.tool && e.session_id == n.session_id));
        if !listed {
            out.push(RosterChange::Upsert(n.clone()));
        }
    }
    out
}

/// What to send when Keo holds `base` for sure and may hold anything in `saved` (an Upsert whose
/// reply never came): `base`'s changes, plus an End for every saved session the runners no
/// longer list, so a session Keo may hold always ends.
pub fn plan(base: &[RosterEntry], saved: &[RosterEntry], new: &[RosterEntry]) -> Vec<RosterChange> {
    let mut out = diff(base, new);
    let same = |a: &RosterEntry, b: &RosterEntry| a.tool == b.tool && a.session_id == b.session_id;
    for o in saved {
        let gone = !new.iter().any(|n| same(n, o));
        let ending = out
            .iter()
            .any(|c| matches!(c, RosterChange::Removed(e) if same(e, o)));
        if gone && !ending {
            out.push(RosterChange::Removed(o.clone()));
        }
    }
    out
}

/// `roster` with `change` applied, keyed as `diff` keys it.
pub fn apply(roster: &mut Vec<RosterEntry>, change: &RosterChange) {
    let same = |a: &RosterEntry, b: &RosterEntry| a.tool == b.tool && a.session_id == b.session_id;
    match change {
        RosterChange::Upsert(e) => match roster.iter_mut().find(|o| same(o, e)) {
            Some(o) => *o = e.clone(),
            None => roster.push(e.clone()),
        },
        RosterChange::Removed(e) => roster.retain(|o| !same(o, e)),
    }
}

/// The Keo event for one change.
pub fn change_event(change: &RosterChange, machine_id: &str) -> SessionEvent {
    match change {
        RosterChange::Upsert(e) => {
            let mut ev = SessionEvent::session(
                &e.session_id,
                EventKind::Register,
                e.tool.wire_name(),
                machine_id,
            );
            ev.tool = Some(e.tool.wire_name());
            ev.cwd = Some(e.cwd.clone());
            ev.name = Some(e.name.clone());
            ev.state = Some(e.state.wire());
            ev.pid = e.pid;
            ev
        }
        RosterChange::Removed(e) => {
            let mut ev = SessionEvent::session(
                &e.session_id,
                EventKind::End,
                e.tool.wire_name(),
                machine_id,
            );
            ev.reason = Some("gone".into());
            ev
        }
    }
}

/// The machine-only Register: no session id, one call per machine. `tool` is the first
/// installed tool (left out when none is installed).
pub fn machine_event(
    machine_id: &str,
    detections: &[Detection],
    approved_dirs: &[String],
) -> SessionEvent {
    let tool = detections
        .iter()
        .find(|d| !matches!(d.status, ToolStatus::NotInstalled))
        .map(|d| d.tool.wire_name());
    SessionEvent::machine_register(machine_id, tool, approved_dirs.to_vec())
}

/// The session Register sent after a task started a new session, so Keo ties the session to
/// the task's Slack thread.
pub fn started_event(
    machine_id: &str,
    tool: ToolKind,
    session_id: &str,
    task_id: i64,
    lease: &str,
    cwd: &str,
) -> SessionEvent {
    let mut ev = SessionEvent::session(
        session_id,
        EventKind::Register,
        tool.wire_name(),
        machine_id,
    );
    ev.tool = Some(tool.wire_name());
    ev.task_id = Some(task_id);
    ev.lease = Some(lease.to_string());
    ev.state = Some(SessionState::Started.wire());
    ev.cwd = Some(cwd.to_string());
    ev.name = Some(session_name(cwd));
    ev
}

/// A short session name: the folder's last part.
pub fn session_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "session".to_string())
}

/// The roster as the runners last reported it, for the panel only. Keo's baseline (`acked`)
/// moves only with changes Keo took; this one moves on every read, so a Keo call that fails never
/// leaves the panel on an old state.
pub struct Shown {
    list: std::sync::Mutex<Option<Vec<RosterEntry>>>,
    tx: tokio::sync::watch::Sender<u64>,
}

impl Default for Shown {
    fn default() -> Self {
        Self {
            list: std::sync::Mutex::new(None),
            tx: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Shown {
    /// Keep `list`; subscribers hear of it only when it differs from the one kept.
    pub fn set(&self, list: &[RosterEntry]) {
        self.set_if(list, || true);
    }

    /// `set`, only while `still()` holds, checked under the same lock `clear` takes: a writer
    /// that checked before a sign-out's fence lands before its `clear`, never after.
    pub fn set_if(&self, list: &[RosterEntry], still: impl FnOnce() -> bool) {
        let mut kept = self.list.lock().expect("shown lock");
        if !still() || kept.as_deref() == Some(list) {
            return;
        }
        *kept = Some(list.to_vec());
        drop(kept);
        self.tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Forget the list (sign-out): the panel shows the saved roster again until the next read.
    pub fn clear(&self) {
        if self.list.lock().expect("shown lock").take().is_some() {
            self.tx.send_modify(|n| *n = n.wrapping_add(1));
        }
    }

    /// The last list read, if any was read since the app started.
    pub fn get(&self) -> Option<Vec<RosterEntry>> {
        self.list.lock().expect("shown lock").clone()
    }

    /// Changes once each time the list changes.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.tx.subscribe()
    }
}

pub struct Registry {
    keo: Arc<KeoClient>,
    store: Arc<Store>,
    runners: Vec<Arc<dyn Runner>>,
    /// Keo sends go one at a time.
    lock: tokio::sync::Mutex<()>,
    /// What Keo holds, per sign-in, moved on by every change it takes (read and written only
    /// under `lock`). `last_roster` is its copy on disk for the next launch: a failed write
    /// never makes a later diff miss a change Keo already took.
    acked: std::sync::Mutex<Option<(u64, Vec<RosterEntry>)>>,
    /// Runner reads go one at a time, apart from Keo sends: each starts from the newest kept
    /// read and lands before the next starts, so none lands out of order, and a slow send never
    /// holds the panel back.
    read_lock: tokio::sync::Mutex<()>,
    /// Machine registrations go out one at a time, each reading the folder list only once it
    /// holds this lock: the one that lands last always carries the newest list.
    register_lock: tokio::sync::Mutex<()>,
    /// What the panel lists.
    pub shown: Shown,
}

impl Registry {
    pub fn new(keo: Arc<KeoClient>, store: Arc<Store>, runners: Vec<Arc<dyn Runner>>) -> Self {
        Self {
            keo,
            store,
            runners,
            lock: tokio::sync::Mutex::new(()),
            acked: std::sync::Mutex::new(None),
            read_lock: tokio::sync::Mutex::new(()),
            register_lock: tokio::sync::Mutex::new(()),
            shown: Shown::default(),
        }
    }

    /// What a runner that cannot list keeps: the panel's last read, else Keo's baseline.
    fn panel_fallback(&self) -> Vec<RosterEntry> {
        self.shown
            .get()
            .unwrap_or_else(|| self.store.get().last_roster)
    }

    /// Show `list` in the panel, unless sign-in `generation` ended while it was read.
    fn show(&self, generation: u64, list: &[RosterEntry]) {
        self.shown
            .set_if(list, || self.keo.sign_in_generation() == generation);
    }

    /// The machine id under sign-in `generation`: `Superseded` once it has ended, checked
    /// atomically with making the id, so a sign-out's wipe is never undone.
    fn machine_id(&self, generation: u64) -> Result<String, KeoError> {
        self.store
            .machine_id_if(|| self.keo.sign_in_generation() == generation)
            .map_err(|e| KeoError::Protocol(e.to_string()))?
            .ok_or(KeoError::Superseded)
    }

    /// Register this machine (approved folders) with Keo. Succeeds only on an accepted reply.
    /// `generation` is the sign-in the caller acts for: once it has ended, nothing is read or
    /// written (reading the machine id would recreate a state.json that sign-out just wiped).
    pub async fn register_machine(
        &self,
        generation: u64,
        detections: &[Detection],
    ) -> Result<(), KeoError> {
        let _guard = self.register_lock.lock().await;
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        let machine_id = self.machine_id(generation)?;
        let dirs: Vec<String> = self
            .store
            .get()
            .approved_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        self.keo
            .send_event(generation, &machine_event(&machine_id, detections, &dirs))
            .await?;
        tracing::info!("machine registered");
        Ok(())
    }

    /// Tell Keo which task a new session belongs to, under the sign-in that pulled the task:
    /// `Superseded` once that sign-in has ended, so one account's task is never registered
    /// with the next account's token.
    pub async fn register_started(
        &self,
        generation: u64,
        tool: ToolKind,
        session_id: &str,
        task_id: i64,
        lease: &str,
        cwd: &str,
    ) -> Result<(), KeoError> {
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        let machine_id = self.machine_id(generation)?;
        self.keo
            .send_event(
                generation,
                &started_event(&machine_id, tool, session_id, task_id, lease, cwd),
            )
            .await
    }

    /// Send one roster event, retrying a transient failure; every try is sent under sign-in
    /// `generation`, so a sign-out meanwhile ends it (`Superseded`). A change that still fails is
    /// not in the in-memory baseline, so the next sync sends it again.
    async fn send_retrying(&self, generation: u64, event: &SessionEvent) -> Result<(), KeoError> {
        let mut backoff =
            crate::doorbell::Backoff::new(Duration::from_secs(2), Duration::from_secs(60));
        let mut attempt = 1;
        loop {
            match self.keo.send_event(generation, event).await {
                Err(e) if attempt < ROSTER_SEND_TRIES && e.transient() => {
                    tracing::info!(error = %e, attempt, "roster event failed; retrying");
                }
                other => return other,
            }
            attempt += 1;
            let wait = backoff.next_delay(&mut rand::thread_rng());
            tokio::time::sleep(wait).await;
        }
    }

    /// The roster now, from every runner. A runner that cannot list keeps its entries from
    /// `last`, so one failed list never reads as all its sessions gone.
    pub async fn current(&self, last: &[RosterEntry]) -> Vec<RosterEntry> {
        let mut all = Vec::new();
        for r in &self.runners {
            match r.list().await {
                Ok(mut list) => all.append(&mut list),
                Err(e) => {
                    tracing::warn!(tool = r.kind().wire_name(), error = %e, "list failed");
                    all.extend(last.iter().filter(|x| x.tool == r.kind()).cloned());
                }
            }
        }
        all
    }

    /// Read the runners for the panel, then send Keo what changed since what it holds; each change
    /// it takes joins that baseline (and its saved copy) at once, so a sync that fails partway
    /// keeps the changes already sent.
    /// `generation` is the sign-in the caller acts for. A sync queued behind another and overtaken
    /// by a sign-out reads nothing (reading the machine id would recreate the state.json sign-out
    /// just wiped); one overtaken while it runs saves nothing more, since the roster it read
    /// belongs to the old account's baseline.
    pub async fn sync(&self, generation: u64) -> Result<usize, KeoError> {
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        {
            let _read = self.read_lock.lock().await;
            // A runner that cannot list keeps its newest known entries, which can be newer than
            // Keo's baseline when an earlier send failed.
            let new = self.current(&self.panel_fallback()).await;
            // The panel follows what the runners say now, whether or not Keo hears it below.
            self.show(generation, &new);
        }
        let _guard = self.lock.lock().await;
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        let machine_id = self.machine_id(generation)?;
        let known = match &*self.acked.lock().expect("acked lock") {
            Some((g, list)) if *g == generation => Some(list.clone()),
            _ => None,
        };
        // Keo gets the newest read kept, never an older one a slower sync made.
        let Some(new) = self.shown.get() else {
            return Err(KeoError::Superseded);
        };
        let full = known.is_none();
        let (mut base, changes) = match known {
            Some(base) => {
                let changes = plan(&base, &self.store.get().last_roster, &new);
                (base, changes)
            }
            // The first sync of a sign-in in this run: the saved copy may trail what Keo took
            // before a crash or a failed write, so every session goes again (Register is
            // idempotent); the saved copy still names the sessions that ended meanwhile.
            None => {
                let base = self.store.get().last_roster;
                let changes = resync(&base, &new);
                (base, changes)
            }
        };
        let current = || self.keo.sign_in_generation() == generation;
        for change in &changes {
            // The saved copy names every session Keo may hold: a new one is saved before it goes,
            // so a crash after Keo took it still ends it at the next launch's re-send; an ended
            // one leaves only once Keo took the End.
            if let RosterChange::Upsert(_) = change {
                self.save_if(&current, |r| apply(r, change))?;
            }
            if let Err(e) = self
                .send_retrying(generation, &change_event(change, &machine_id))
                .await
            {
                // The change may or may not have reached Keo (a lost reply): no baseline is
                // trusted, and the next sync re-sends every session and ends every saved one the
                // runners no longer list.
                *self.acked.lock().expect("acked lock") = None;
                return Err(e);
            }
            // Each change Keo took joins the in-memory baseline at once: a later failed send or
            // write leaves it matching what Keo holds, so the next sync corrects the rest. A full
            // re-send that stops partway leaves none, so the next sync re-sends everything again.
            apply(&mut base, change);
            if !full {
                *self.acked.lock().expect("acked lock") = Some((generation, base.clone()));
            }
            if let RosterChange::Removed(_) = change {
                self.save_if(&current, |r| apply(r, change))?;
            }
        }
        *self.acked.lock().expect("acked lock") = Some((generation, new.clone()));
        // A newer `active_at` alone sends nothing but is still saved, so the panel's order holds.
        if self.store.get().last_roster != new {
            self.save_if(&current, |r| *r = new)?;
        }
        Ok(changes.len())
    }

    /// Change the saved roster while sign-in `current` holds and the state still has its machine
    /// id (a sign-out's wipe removes it), both checked under the store lock the wipe takes;
    /// `Superseded` otherwise. A failed write fails the sync; the in-memory baseline (`acked`)
    /// still holds what Keo took, and the next launch re-sends every session (`resync`).
    fn save_if(
        &self,
        current: &impl Fn() -> bool,
        f: impl FnOnce(&mut Vec<RosterEntry>),
    ) -> Result<(), KeoError> {
        match self.store.update_if(
            |p| p.machine_id.is_some() && current(),
            |p| f(&mut p.last_roster),
        ) {
            Ok(true) => Ok(()),
            Ok(false) => Err(KeoError::Superseded),
            Err(e) => Err(KeoError::Protocol(format!("roster not saved: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, state: SessionState) -> RosterEntry {
        RosterEntry {
            tool: ToolKind::Codex,
            session_id: id.into(),
            cwd: "/w".into(),
            name: "n".into(),
            state,
            pid: Some(1),
            active_at: Some(100),
        }
    }

    #[test]
    fn diff_sends_only_changes() {
        use SessionState::*;
        let old = vec![entry("aaaa1111", Idle), entry("bbbb2222", Idle)];
        let new = vec![
            entry("aaaa1111", Idle),
            entry("bbbb2222", Working),
            entry("cccc3333", Idle),
        ];
        let d = diff(&old, &new);
        assert_eq!(
            d,
            vec![
                RosterChange::Upsert(entry("bbbb2222", Working)),
                RosterChange::Upsert(entry("cccc3333", Idle)),
            ]
        );
        assert_eq!(
            diff(&new, &[entry("aaaa1111", Idle)]),
            vec![
                RosterChange::Removed(entry("bbbb2222", Working)),
                RosterChange::Removed(entry("cccc3333", Idle)),
            ]
        );
        assert!(diff(&old, &old).is_empty());
    }

    #[test]
    fn a_newer_activity_time_alone_sends_nothing() {
        let old = vec![entry("aaaa1111", SessionState::Idle)];
        let mut later = entry("aaaa1111", SessionState::Idle);
        later.active_at = Some(200);
        assert!(diff(&old, &[later.clone()]).is_empty());
        later.state = SessionState::Working;
        assert_eq!(
            diff(&old, &[later.clone()]),
            vec![RosterChange::Upsert(later)]
        );
    }

    #[test]
    fn the_panel_list_changes_and_notifies_only_on_a_real_change() {
        let shown = Shown::default();
        let mut rx = shown.subscribe();
        assert_eq!(shown.get(), None);
        let waiting = vec![entry("aaaa1111", SessionState::WaitingPermission)];
        shown.set(&waiting);
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        assert_eq!(shown.get(), Some(waiting.clone()));
        shown.set(&waiting);
        assert!(!rx.has_changed().unwrap(), "the same list redraws nothing");
        let mut later = waiting[0].clone();
        later.active_at = Some(300);
        shown.set(std::slice::from_ref(&later));
        assert!(rx.has_changed().unwrap(), "a newer time reorders the panel");
        rx.borrow_and_update();
        shown.clear();
        assert!(
            rx.has_changed().unwrap(),
            "a sign-out redraws the saved roster"
        );
        assert_eq!(shown.get(), None);
        shown.set_if(&waiting, || false);
        assert_eq!(shown.get(), None, "an ended sign-in's read is dropped");
    }

    #[test]
    fn each_acknowledged_change_moves_the_baseline_toward_the_new_roster() {
        use SessionState::*;
        let last = vec![entry("aaaa1111", Idle), entry("bbbb2222", Idle)];
        let new = vec![
            entry("aaaa1111", WaitingPermission),
            entry("cccc3333", Idle),
        ];
        let changes = diff(&last, &new);
        let mut base = last.clone();
        apply(&mut base, &changes[0]);
        // Only the first send landed: the baseline holds it, so the next diff sends the rest.
        assert_eq!(base[0], entry("aaaa1111", WaitingPermission));
        assert_eq!(diff(&base, &new), changes[1..].to_vec());
        for c in &changes[1..] {
            apply(&mut base, c);
        }
        assert!(diff(&base, &new).is_empty());
    }

    #[test]
    fn a_first_sync_sends_every_session_again_and_still_removes_the_ended() {
        use SessionState::*;
        let saved = vec![entry("aaaa1111", Idle), entry("bbbb2222", Idle)];
        let now = vec![entry("aaaa1111", Idle), entry("cccc3333", Working)];
        let changes = resync(&saved, &now);
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&RosterChange::Upsert(entry("aaaa1111", Idle))));
        assert!(changes.contains(&RosterChange::Upsert(entry("cccc3333", Working))));
        assert!(changes.contains(&RosterChange::Removed(entry("bbbb2222", Idle))));
    }

    #[test]
    fn a_session_keo_may_hold_ends_even_when_its_register_went_unanswered() {
        use SessionState::*;
        // Keo surely holds aaaa; bbbb's Register was saved first but its reply never came.
        let base = vec![entry("aaaa1111", Idle)];
        let saved = vec![entry("aaaa1111", Idle), entry("bbbb2222", Idle)];
        let gone_both: Vec<RosterEntry> = vec![];
        let changes = plan(&base, &saved, &gone_both);
        assert_eq!(
            changes,
            vec![
                RosterChange::Removed(entry("aaaa1111", Idle)),
                RosterChange::Removed(entry("bbbb2222", Idle)),
            ]
        );
        // Still listed: it goes again as an Upsert, and nothing ends.
        let still = vec![entry("aaaa1111", Idle), entry("bbbb2222", Idle)];
        assert_eq!(
            plan(&base, &saved, &still),
            vec![RosterChange::Upsert(entry("bbbb2222", Idle))]
        );
    }

    #[test]
    fn same_id_other_tool_is_distinct() {
        let mut claude = entry("aaaa1111", SessionState::Idle);
        claude.tool = ToolKind::ClaudeCode;
        let d = diff(&[entry("aaaa1111", SessionState::Idle)], &[claude]);
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn events_for_changes() {
        let up = change_event(
            &RosterChange::Upsert(entry("aaaa1111", SessionState::WaitingPermission)),
            "m1234567",
        );
        let v = up.to_args();
        assert_eq!(v["event"], "Register");
        assert_eq!(v["session_id"], "aaaa1111");
        assert_eq!(v["agent"], "codex");
        assert_eq!(v["tool"], "codex");
        assert_eq!(v["state"], "waiting_permission");
        assert_eq!(v["machine_id"], "m1234567");
        let gone = change_event(
            &RosterChange::Removed(entry("aaaa1111", SessionState::Idle)),
            "m1234567",
        );
        assert_eq!(gone.to_args()["event"], "End");
    }

    #[test]
    fn machine_register_has_no_session() {
        let dets = vec![
            Detection {
                tool: ToolKind::Codex,
                status: ToolStatus::NotInstalled,
            },
            Detection {
                tool: ToolKind::ClaudeCode,
                status: ToolStatus::TooOld {
                    path: "/x".into(),
                    version: "1.0.0".into(),
                },
            },
        ];
        let v = machine_event("m1234567", &dets, &["/w".into()]).to_args();
        assert!(v.get("session_id").is_none());
        assert!(v.get("state").is_none());
        assert_eq!(v["tool"], "claude-code");
        assert_eq!(v["approved_dirs"][0], "/w");
        let none = machine_event("m1234567", &dets[..1], &[]).to_args();
        assert!(none.get("tool").is_none());
    }

    #[test]
    fn started_register_ties_session_to_task() {
        let v = started_event(
            "m1234567",
            ToolKind::Codex,
            "sess-real-1",
            42,
            "l-42",
            "/w/repo",
        )
        .to_args();
        assert_eq!(v["lease"], "l-42");
        assert_eq!(v["event"], "Register");
        assert_eq!(v["session_id"], "sess-real-1");
        assert_eq!(v["task_id"], 42);
        assert_eq!(v["state"], "started");
        assert_eq!(v["name"], "repo");
        assert_eq!(v["cwd"], "/w/repo");
        assert_eq!(session_name("/"), "session");
    }
}
