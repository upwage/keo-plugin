//! The app state machine (a pure transition function) and the persisted `state.json`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::runner::RosterEntry;

/// Where the app is. See `transition` for the edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    FirstRun,
    SignedOut,
    Registering,
    Listening,
    Paused,
    Pulling,
    Acting,
    Relaying,
    NeedsUpdate,
}

/// What can happen to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The person finished or skipped the welcome.
    FirstRunDone,
    SignedIn,
    SignInFailed,
    /// Registration with Keo succeeded. (Pause is not an event: it is a separate flag, see
    /// `Snapshot`, so no activity or version change can drop it.)
    Registered,
    /// The doorbell rang.
    Ring,
    /// A task was pulled and handed to a runner.
    TaskStarted,
    /// The ring had nothing to pull (or the task was refused).
    NothingToDo,
    /// The running session waits for a person.
    NeedsInput,
    /// The person answered; the session runs again.
    InputGiven,
    /// The session finished its turn.
    TaskDone,
    /// A found tool is older than its floor.
    ToolOutOfRange,
    /// Every found tool is in range again.
    ToolsInRange,
    SignOut,
    /// Keo answered 401 after one refresh.
    Unauthorized,
}

/// The tray icon each state shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrayState {
    SignedOut,
    Listening,
    Paused,
    Working,
    NeedsAttention,
    NeedsUpdate,
}

impl TrayState {
    /// The file under icons/tray/.
    pub fn icon_name(self) -> &'static str {
        match self {
            TrayState::SignedOut => "signed-out",
            TrayState::Listening => "listening",
            TrayState::Paused => "paused",
            TrayState::Working => "working",
            TrayState::NeedsAttention => "needs-attention",
            TrayState::NeedsUpdate => "needs-update",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            TrayState::SignedOut => "Keo Companion: signed out",
            TrayState::Listening => "Keo Companion: listening",
            TrayState::Paused => "Keo Companion: paused",
            TrayState::Working => "Keo Companion: working",
            TrayState::NeedsAttention => "Keo Companion: needs your attention",
            TrayState::NeedsUpdate => {
                "Keo Companion: a coding tool or the companion needs an update"
            }
        }
    }
}

impl AppState {
    pub fn tray(self) -> TrayState {
        match self {
            AppState::FirstRun | AppState::SignedOut => TrayState::SignedOut,
            AppState::Registering | AppState::Listening | AppState::Pulling => TrayState::Listening,
            AppState::Paused => TrayState::Paused,
            AppState::Acting => TrayState::Working,
            AppState::Relaying => TrayState::NeedsAttention,
            AppState::NeedsUpdate => TrayState::NeedsUpdate,
        }
    }

    /// Whether the doorbell should poll in this state.
    pub fn polls(self) -> bool {
        matches!(
            self,
            AppState::Listening | AppState::Pulling | AppState::Acting | AppState::Relaying
        )
    }

    pub fn signed_in(self) -> bool {
        !matches!(self, AppState::FirstRun | AppState::SignedOut)
    }
}

/// The pure transition function. An event that does not apply leaves the state as it is.
pub fn transition(state: AppState, event: Event) -> AppState {
    use AppState as S;
    use Event as E;
    match (state, event) {
        // Sign out and a final 401 win from anywhere.
        (_, E::SignOut) | (_, E::Unauthorized) => S::SignedOut,

        (S::FirstRun, E::FirstRunDone) | (S::FirstRun, E::SignInFailed) => S::SignedOut,
        (S::FirstRun | S::SignedOut, E::SignedIn) => S::Registering,

        (S::Registering, E::Registered) => S::Listening,

        (S::Listening, E::Ring) => S::Pulling,
        (S::Pulling, E::TaskStarted) => S::Acting,
        // A session Keo handed work to starts a new turn, or asks a question, after the machine
        // went back to listening.
        (S::Listening, E::TaskStarted) => S::Acting,
        (S::Listening, E::NeedsInput) => S::Relaying,
        (S::Pulling, E::NothingToDo) => S::Listening,
        (S::Acting, E::NeedsInput) => S::Relaying,
        (S::Relaying, E::InputGiven) => S::Acting,
        (S::Acting | S::Relaying, E::TaskDone) => S::Listening,

        (S::Listening | S::Registering, E::ToolOutOfRange) => S::NeedsUpdate,
        (S::NeedsUpdate, E::ToolsInRange) => S::Listening,

        (s, _) => s,
    }
}

/// The activity state plus the pause switch. The two are independent: pause survives every
/// activity and version change, and everything that must honor pause reads this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub activity: AppState,
    pub paused: bool,
    /// Keo confirmed this machine's registration since the last sign-in. Tracked apart from
    /// tool health: a version change can never stand in for a registration.
    pub registered: bool,
    /// The last tool check found every installed tool in range. Also tracked apart from the
    /// activity, so a check that lands mid-pull is not lost when the pull ends.
    pub tools_ok: bool,
}

/// Listening needs a confirmed registration (else Registering) and tools in range (else
/// NeedsUpdate).
pub fn settle(next: AppState, registered: bool, tools_ok: bool) -> AppState {
    match next {
        AppState::Listening if !registered => AppState::Registering,
        AppState::Listening if !tools_ok => AppState::NeedsUpdate,
        other => other,
    }
}

impl Snapshot {
    /// The state shown to the person. Pause shows over idle states; a session that is already
    /// running (Acting, Relaying) still shows as such until it ends.
    pub fn display(self) -> AppState {
        let idle = matches!(
            self.activity,
            AppState::Listening | AppState::Pulling | AppState::NeedsUpdate
        );
        if self.paused && idle {
            AppState::Paused
        } else {
            self.activity
        }
    }

    pub fn tray(self) -> TrayState {
        self.display().tray()
    }

    /// The doorbell polls only when the activity allows it and the switch is off.
    pub fn polls(self) -> bool {
        !self.paused && self.activity.polls()
    }

    /// A ring may be pulled, and an answer delivered to a session already running, only when
    /// not paused.
    pub fn accepts_work(self) -> bool {
        !self.paused && self.activity.signed_in()
    }

    /// A pulled task may be delivered only when not paused, registered, and every tool is in
    /// range: a tool that needs updating stops new work, as the panel says.
    pub fn accepts_tasks(self) -> bool {
        self.accepts_work() && self.registered && self.tools_ok
    }

    pub fn signed_in(self) -> bool {
        self.activity.signed_in()
    }

    /// Signed in, and Keo has not confirmed the registration yet: keep retrying.
    pub fn needs_registration(self) -> bool {
        self.signed_in() && !self.registered
    }
}

/// The live state, shared by every task. Each activity change goes through `transition`.
pub struct Machine {
    tx: watch::Sender<Snapshot>,
}

impl Machine {
    pub fn new(initial: AppState, paused: bool) -> Self {
        let (tx, _) = watch::channel(Snapshot {
            activity: initial,
            paused,
            registered: false,
            tools_ok: true,
        });
        Self { tx }
    }

    /// The activity state (without pause).
    pub fn get(&self) -> AppState {
        self.tx.borrow().activity
    }

    pub fn snapshot(&self) -> Snapshot {
        *self.tx.borrow()
    }

    pub fn paused(&self) -> bool {
        self.tx.borrow().paused
    }

    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.tx.subscribe()
    }

    pub fn set_paused(&self, paused: bool) {
        self.tx.send_if_modified(|s| {
            let changed = s.paused != paused;
            if changed {
                tracing::info!(paused, "pause switch");
            }
            s.paused = paused;
            changed
        });
    }

    /// Apply an event; returns the new activity state.
    pub fn apply(&self, event: Event) -> AppState {
        let mut out = self.get();
        self.tx.send_if_modified(|s| {
            let registered = match event {
                Event::Registered => true,
                Event::SignedIn | Event::SignOut | Event::Unauthorized => false,
                _ => s.registered,
            };
            let tools_ok = match event {
                Event::ToolOutOfRange => false,
                Event::ToolsInRange => true,
                _ => s.tools_ok,
            };
            let next = settle(transition(s.activity, event), registered, tools_ok);
            let changed =
                next != s.activity || registered != s.registered || tools_ok != s.tools_ok;
            if next != s.activity {
                tracing::info!(from = ?s.activity, to = ?next, event = ?event, "state");
            }
            s.activity = next;
            s.registered = registered;
            s.tools_ok = tools_ok;
            out = next;
            changed
        });
        out
    }
}

// ---- persisted state ----

/// Everything the app keeps between launches, in `<app data dir>/state.json`.
/// No tokens here: the refresh token lives in the keychain, the access token in memory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub machine_id: Option<String>,
    pub client_id: Option<String>,
    pub approved_dirs: Vec<PathBuf>,
    pub start_at_login: bool,
    /// Start at login was set by hand: the first sign-in then keeps that choice instead of
    /// turning it on.
    pub start_at_login_chosen: bool,
    pub first_run_done: bool,
    pub paused: bool,
    pub last_roster: Vec<RosterEntry>,
    /// The inbox's task ledger (`inbox::Ledger::kept`), so a restart never runs a task twice.
    pub inbox_tasks: Vec<KeptTask>,
}

/// One task the inbox handled: its ack outcome (`delivered`, `started`, `failed`) and whether Keo
/// confirmed that ack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptTask {
    pub task_id: i64,
    pub outcome: String,
    pub detail: Option<String>,
    pub confirmed: bool,
}

/// A fresh random machine id: 24 random bytes as unpadded base64url (32 chars), which matches
/// `^[A-Za-z0-9_-]{8,128}$`.
pub fn new_machine_id() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// `^[A-Za-z0-9_-]{8,128}$`, the id shape Keo's MCP server accepts.
pub fn is_valid_id(id: &str) -> bool {
    (8..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The persisted state and its file. All writes are atomic (temp file + rename).
pub struct Store {
    path: PathBuf,
    data: Mutex<Persisted>,
}

impl Store {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("state.json");
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "state.json unreadable, starting fresh");
                Persisted::default()
            }),
            Err(_) => Persisted::default(),
        };
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    pub fn get(&self) -> Persisted {
        self.data.lock().expect("state lock").clone()
    }

    /// Change the state and write it. The change is made on a copy and goes live only after
    /// the write succeeds; on an error memory stays as it was (matching the file). The lock is
    /// held across the write, so writes and `wipe` never interleave.
    pub fn update<T>(&self, f: impl FnOnce(&mut Persisted) -> T) -> std::io::Result<T> {
        let mut guard = self.data.lock().expect("state lock");
        let mut next = guard.clone();
        let out = f(&mut next);
        write_atomic(&self.path, &serde_json::to_vec_pretty(&next)?)?;
        *guard = next;
        Ok(out)
    }

    /// `update`, only while `current` holds for the state now, checked under the lock `wipe`
    /// takes (as `machine_id_if`): a stale caller never writes after a sign-out's wipe.
    /// `Ok(false)` when it did not write.
    pub fn update_if(
        &self,
        current: impl FnOnce(&Persisted) -> bool,
        f: impl FnOnce(&mut Persisted),
    ) -> std::io::Result<bool> {
        let mut guard = self.data.lock().expect("state lock");
        if !current(&guard) {
            return Ok(false);
        }
        let mut next = guard.clone();
        f(&mut next);
        write_atomic(&self.path, &serde_json::to_vec_pretty(&next)?)?;
        *guard = next;
        Ok(true)
    }

    /// The machine id, made (and written) on first use.
    pub fn machine_id(&self) -> std::io::Result<String> {
        if let Some(id) = self.get().machine_id {
            return Ok(id);
        }
        self.update(|p| p.machine_id.get_or_insert_with(new_machine_id).to_string())
    }

    /// The machine id for a caller acting for a sign-in, or `None` once `current` says that
    /// sign-in has ended. Checked and made under the lock `wipe` takes: sign-out moves the
    /// sign-in on before it wipes, so a stale caller never writes a new id after the wipe.
    pub fn machine_id_if(&self, current: impl Fn() -> bool) -> std::io::Result<Option<String>> {
        let mut guard = self.data.lock().expect("state lock");
        if !current() {
            return Ok(None);
        }
        if let Some(id) = &guard.machine_id {
            return Ok(Some(id.clone()));
        }
        let mut next = guard.clone();
        let id = next
            .machine_id
            .get_or_insert_with(new_machine_id)
            .to_string();
        write_atomic(&self.path, &serde_json::to_vec_pretty(&next)?)?;
        *guard = next;
        Ok(Some(id))
    }

    /// Forget everything: the file goes and memory resets. A new machine id comes at the next
    /// sign-in.
    /// Serialized with `update` by the same lock; memory resets only when the file is gone.
    pub fn wipe(&self) -> std::io::Result<()> {
        let mut guard = self.data.lock().expect("state lock");
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        *guard = Persisted::default();
        Ok(())
    }
}

/// Sign out locally: forget the saved state, then bring the live state back in line with it
/// (signed out, pause off as the fresh default), so nothing live outlives the wipe. When the
/// wipe fails, the live pause keeps matching what is still on disk.
pub fn forget_all(store: &Store, machine: &Machine) -> std::io::Result<()> {
    // Signed out before the wipe: a poll that checks the state under the store lock then never
    // writes a machine id after the file is gone.
    machine.apply(Event::SignOut);
    let wiped = store.wipe();
    machine.set_paused(store.get().paused);
    wiped
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use AppState as S;
    use Event as E;

    #[test]
    fn machine_id_is_not_made_after_its_sign_in_ended() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path());
        assert_eq!(store.machine_id_if(|| false).unwrap(), None);
        assert!(
            store.get().machine_id.is_none(),
            "nothing written for a stale caller"
        );
        let id = store.machine_id_if(|| true).unwrap().unwrap();
        assert_eq!(store.machine_id_if(|| true).unwrap(), Some(id));
    }

    #[test]
    fn a_stale_caller_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path());
        assert!(!store.update_if(|_| false, |p| p.paused = true).unwrap());
        assert!(!store.get().paused);
        assert!(!dir.path().join("state.json").exists(), "no file made");
        assert!(store.update_if(|_| true, |p| p.paused = true).unwrap());
        assert!(Store::load(dir.path()).get().paused);
    }

    #[test]
    fn a_bound_session_starting_again_moves_listening_back_to_work() {
        assert_eq!(transition(S::Listening, E::TaskStarted), S::Acting);
        assert_eq!(transition(S::Listening, E::NeedsInput), S::Relaying);
    }

    #[test]
    fn happy_path() {
        let mut s = S::FirstRun;
        for (e, want) in [
            (E::SignedIn, S::Registering),
            (E::Registered, S::Listening),
            (E::Ring, S::Pulling),
            (E::TaskStarted, S::Acting),
            (E::NeedsInput, S::Relaying),
            (E::InputGiven, S::Acting),
            (E::NeedsInput, S::Relaying),
            (E::TaskDone, S::Listening),
        ] {
            s = transition(s, e);
            assert_eq!(s, want, "after {e:?}");
        }
    }

    #[test]
    fn pause_and_resume() {
        // Pause is a flag beside the activity, never lost to an activity or version change.
        let m = Machine::new(S::Registering, true);
        m.apply(E::Registered);
        assert_eq!(m.snapshot().display(), S::Paused);
        assert!(!m.snapshot().polls());
        for e in [E::Ring, E::TaskStarted, E::NeedsInput, E::TaskDone] {
            m.apply(e);
            assert!(m.paused(), "pause lost after {e:?}");
        }
        m.apply(E::ToolOutOfRange);
        m.apply(E::ToolsInRange);
        assert!(m.paused());
        assert_eq!(m.snapshot().display(), S::Paused);
        // Pause pressed mid-task: kept, and no new work is accepted.
        let m = Machine::new(S::SignedOut, false);
        m.apply(E::SignedIn);
        m.apply(E::Registered);
        m.apply(E::Ring);
        m.apply(E::TaskStarted);
        m.set_paused(true);
        assert!(!m.snapshot().accepts_work());
        assert!(!m.snapshot().polls());
        assert_eq!(m.snapshot().display(), S::Acting);
        m.apply(E::TaskDone);
        assert_eq!(m.snapshot().display(), S::Paused);
        m.set_paused(false);
        assert_eq!(m.snapshot().display(), S::Listening);
        assert!(m.snapshot().polls());
    }

    #[test]
    fn sign_out_and_unauthorized_from_anywhere() {
        for s in [
            S::FirstRun,
            S::SignedOut,
            S::Registering,
            S::Listening,
            S::Paused,
            S::Pulling,
            S::Acting,
            S::Relaying,
            S::NeedsUpdate,
        ] {
            assert_eq!(transition(s, E::SignOut), S::SignedOut);
            assert_eq!(transition(s, E::Unauthorized), S::SignedOut);
        }
    }

    #[test]
    fn first_run_and_sign_in() {
        assert_eq!(transition(S::FirstRun, E::FirstRunDone), S::SignedOut);
        assert_eq!(transition(S::FirstRun, E::SignInFailed), S::SignedOut);
        assert_eq!(transition(S::SignedOut, E::SignInFailed), S::SignedOut);
        assert_eq!(transition(S::SignedOut, E::SignedIn), S::Registering);
        assert_eq!(transition(S::SignedOut, E::Ring), S::SignedOut);
    }

    #[test]
    fn needs_update_round_trip() {
        assert_eq!(transition(S::Listening, E::ToolOutOfRange), S::NeedsUpdate);
        assert_eq!(transition(S::NeedsUpdate, E::Ring), S::NeedsUpdate);
        assert_eq!(transition(S::NeedsUpdate, E::ToolsInRange), S::Listening);
        assert_eq!(transition(S::Acting, E::ToolOutOfRange), S::Acting);
    }

    #[test]
    fn empty_ring_goes_back_to_listening() {
        assert_eq!(transition(S::Pulling, E::NothingToDo), S::Listening);
    }

    #[test]
    fn tray_mapping() {
        assert_eq!(S::SignedOut.tray(), TrayState::SignedOut);
        assert_eq!(S::FirstRun.tray(), TrayState::SignedOut);
        assert_eq!(S::Registering.tray(), TrayState::Listening);
        assert_eq!(S::Pulling.tray(), TrayState::Listening);
        assert_eq!(S::Paused.tray(), TrayState::Paused);
        assert_eq!(S::Acting.tray(), TrayState::Working);
        assert_eq!(S::Relaying.tray(), TrayState::NeedsAttention);
        assert_eq!(S::NeedsUpdate.tray(), TrayState::NeedsUpdate);
    }

    #[test]
    fn polls_only_when_signed_in_and_running() {
        assert!(!S::SignedOut.polls());
        assert!(!S::NeedsUpdate.polls());
        assert!(S::Listening.polls());
    }

    #[test]
    fn machine_notifies_on_change_only() {
        let m = Machine::new(S::SignedOut, false);
        let mut rx = m.subscribe();
        rx.borrow_and_update();
        assert_eq!(m.apply(E::Ring), S::SignedOut);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(m.apply(E::SignedIn), S::Registering);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn listening_needs_a_confirmed_registration() {
        let m = Machine::new(S::SignedOut, false);
        m.apply(E::SignedIn);
        assert!(m.snapshot().needs_registration());
        // The tool check flips versions while registration is still failing.
        m.apply(E::ToolOutOfRange);
        assert_eq!(m.get(), S::NeedsUpdate);
        m.apply(E::ToolsInRange);
        assert_eq!(m.get(), S::Registering);
        assert!(m.snapshot().needs_registration());
        assert!(!m.snapshot().polls());
        // Registration lands while a tool is out of range: registered, still NeedsUpdate.
        m.apply(E::ToolOutOfRange);
        m.apply(E::Registered);
        assert_eq!(m.get(), S::NeedsUpdate);
        assert!(!m.snapshot().needs_registration());
        m.apply(E::ToolsInRange);
        assert_eq!(m.get(), S::Listening);
        // A new sign-in needs a new registration.
        m.apply(E::SignOut);
        m.apply(E::SignedIn);
        assert!(m.snapshot().needs_registration());
        assert_eq!(settle(S::Listening, false, true), S::Registering);
        assert_eq!(settle(S::Listening, true, true), S::Listening);
        assert_eq!(settle(S::Listening, true, false), S::NeedsUpdate);
    }

    #[test]
    fn a_tool_check_mid_pull_is_kept_and_stops_new_tasks() {
        let m = Machine::new(S::SignedOut, false);
        m.apply(E::SignedIn);
        m.apply(E::Registered);
        m.apply(E::Ring);
        assert!(m.snapshot().accepts_tasks());
        // The hourly check lands while the pull runs: the pull is not interrupted...
        assert_eq!(m.apply(E::ToolOutOfRange), S::Pulling);
        // ...but the task it brings is refused, and the end of the pull shows NeedsUpdate.
        assert!(!m.snapshot().accepts_tasks());
        assert!(m.snapshot().accepts_work());
        assert_eq!(m.apply(E::NothingToDo), S::NeedsUpdate);
        assert_eq!(m.apply(E::ToolsInRange), S::Listening);
        assert!(m.snapshot().accepts_tasks());
    }

    #[test]
    fn pause_does_not_outlive_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path());
        store.update(|p| p.paused = true).unwrap();
        let m = Machine::new(S::Listening, true);
        forget_all(&store, &m).unwrap();
        assert_eq!(m.get(), S::SignedOut);
        assert!(!m.paused());
        // Sign in again: live and saved agree, so a relaunch shows the same thing.
        m.apply(E::SignedIn);
        m.apply(E::Registered);
        assert_eq!(m.paused(), Store::load(dir.path()).get().paused);
        assert_eq!(m.snapshot().display(), S::Listening);
    }

    #[test]
    fn failed_write_leaves_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path());
        // A directory where state.json should be makes every write fail.
        std::fs::create_dir_all(dir.path().join("state.json").join("x")).unwrap();
        assert!(store.machine_id().is_err());
        assert_eq!(store.get().machine_id, None);
        assert!(store.update(|p| p.paused = true).is_err());
        assert!(!store.get().paused);
        // wipe fails too (non-empty dir), and memory is untouched.
        let before = store.get();
        assert!(store.wipe().is_err());
        assert_eq!(store.get(), before);
    }

    #[test]
    fn machine_id_shape() {
        let id = new_machine_id();
        assert_eq!(id.len(), 32);
        assert!(is_valid_id(&id));
        assert_ne!(id, new_machine_id());
        assert!(!is_valid_id("short"));
        assert!(!is_valid_id("has space in it"));
    }

    #[test]
    fn store_round_trip_and_wipe() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load(dir.path());
        let id = store.machine_id().unwrap();
        store
            .update(|p| {
                p.paused = true;
                p.client_id = Some("client-one".into());
            })
            .unwrap();
        let again = Store::load(dir.path());
        assert_eq!(again.get().machine_id.as_deref(), Some(id.as_str()));
        assert!(again.get().paused);
        again.wipe().unwrap();
        assert!(!dir.path().join("state.json").exists());
        assert_eq!(Store::load(dir.path()).get(), Persisted::default());
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
