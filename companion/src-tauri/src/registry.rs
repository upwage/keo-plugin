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

/// What changed from `old` to `new`, keyed by (tool, session id).
pub fn diff(old: &[RosterEntry], new: &[RosterEntry]) -> Vec<RosterChange> {
    let key = |e: &RosterEntry| (e.tool, e.session_id.clone());
    let mut out = Vec::new();
    for n in new {
        match old.iter().find(|o| key(o) == key(n)) {
            Some(o) if o == n => {}
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

pub struct Registry {
    keo: Arc<KeoClient>,
    store: Arc<Store>,
    runners: Vec<Arc<dyn Runner>>,
    lock: tokio::sync::Mutex<()>,
    /// Machine registrations go out one at a time, each reading the folder list only once it
    /// holds this lock: the one that lands last always carries the newest list.
    register_lock: tokio::sync::Mutex<()>,
}

impl Registry {
    pub fn new(keo: Arc<KeoClient>, store: Arc<Store>, runners: Vec<Arc<dyn Runner>>) -> Self {
        Self {
            keo,
            store,
            runners,
            lock: tokio::sync::Mutex::new(()),
            register_lock: tokio::sync::Mutex::new(()),
        }
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
    /// `generation`, so a sign-out meanwhile ends it (`Superseded`). A sync that still fails saves nothing, so the next sync
    /// sends the same changes again.
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

    /// Send what changed since the last sync; save the new roster once every change is sent.
    /// `generation` is the sign-in the caller acts for. A sync queued behind another and overtaken
    /// by a sign-out reads nothing (reading the machine id would recreate the state.json sign-out
    /// just wiped); one overtaken while it runs saves nothing, since the roster it read belongs
    /// to the old account's baseline.
    pub async fn sync(&self, generation: u64) -> Result<usize, KeoError> {
        let _guard = self.lock.lock().await;
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        let machine_id = self.machine_id(generation)?;
        let last = self.store.get().last_roster;
        let new = self.current(&last).await;
        let changes = diff(&last, &new);
        for change in &changes {
            self.send_retrying(generation, &change_event(change, &machine_id))
                .await?;
        }
        if self.keo.sign_in_generation() != generation {
            return Err(KeoError::Superseded);
        }
        if !changes.is_empty() {
            if let Err(e) = self.store.update(|p| p.last_roster = new) {
                tracing::warn!(error = %e, "roster not saved");
            }
        }
        Ok(changes.len())
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
