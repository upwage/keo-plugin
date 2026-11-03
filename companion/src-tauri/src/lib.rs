//! Keo Companion: a menu-bar app that connects Keo to the coding agents on this machine.
//!
//! Wiring: `run()` builds the app, `setup` builds the `Core` (store, auth, MCP client, runners,
//! registry, relay, inbox) and starts the long-running tasks (doorbell, inbox, state watcher,
//! hourly checks). The panel talks to the core only through the commands below.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, PhysicalSize,
    Rect, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::mpsc;

pub mod auth;
pub mod dirs;
pub mod doorbell;
pub mod inbox;
pub mod keo;
pub mod log;
pub mod registry;
pub mod relay;
pub mod runner;
pub mod state;
pub mod versions;

use auth::Auth;
use keo::{BaseUrl, KeoClient};
use registry::Registry;
use relay::Relay;
use runner::{Detection, RosterEntry, Runner, ToolStatus};
use state::{AppState, Event, Machine, Store, TrayState};

const PANEL: &str = "panel";
const TRAY_ID: &str = "main";
const STATUS_EVENT: &str = "status";
const PANEL_W: f64 = 380.0;
const PANEL_H: f64 = 560.0;
const HOURLY: Duration = Duration::from_secs(3600);

/// Everything the commands and background tasks share.
pub struct Core {
    app: AppHandle,
    base: BaseUrl,
    store: Arc<Store>,
    machine: Arc<Machine>,
    auth: Arc<Auth>,
    keo: Arc<KeoClient>,
    registry: Arc<Registry>,
    inbox: Arc<inbox::Inbox>,
    detections: Mutex<Vec<Detection>>,
    runners: Vec<Arc<dyn Runner>>,
    log_dir: PathBuf,
    signing_in: AtomicBool,
    /// Counts Sign in clicks, so a click queued behind a newer one never runs.
    sign_in_attempts: std::sync::atomic::AtomicU64,
    /// Sign-in and sign-out run one at a time: a sign-out's cleanup can never wipe what a newer
    /// sign-in just saved (client id, machine id, keychain), and a sign-in never persists while
    /// a sign-out is still clearing. Registration retries run after it is released.
    lifecycle: tokio::sync::Mutex<()>,
    /// Held by each settings command for its whole change, and by sign-out across disabling
    /// start at login and wiping state.json: a settings edit then lands wholly before the wipe
    /// (and is wiped) or wholly after it (a signed-out edit), never in between. A plain lock,
    /// not `lifecycle`, so a toggle never waits on a browser sign-in.
    settings: Mutex<()>,
    /// Counts folder edits, so only the newest one keeps retrying its update to Keo.
    folder_edits: std::sync::atomic::AtomicU64,
    last_error: Mutex<Option<String>>,
    /// The tray icon's rect at the last click, and the cursor then (see [`TrayClick`]).
    tray_rect: Mutex<Option<TrayClick>>,
}

#[derive(Serialize, Clone)]
struct ToolView {
    tool: &'static str,
    name: &'static str,
    installed: bool,
    version: Option<String>,
    /// A line for the panel: "not installed", "Update Codex to continue", or empty.
    notice: String,
}

#[derive(Serialize, Clone)]
struct StatusView {
    state: AppState,
    tray: TrayState,
    signed_in: bool,
    signing_in: bool,
    paused: bool,
    start_at_login: bool,
    first_run_done: bool,
    approved_dirs: Vec<String>,
    tools: Vec<ToolView>,
    sessions: Vec<RosterEntry>,
    host: String,
    version: String,
    error: Option<String>,
}

fn tool_view(d: &Detection) -> ToolView {
    let name = d.tool.display_name();
    let (installed, version, notice) = match &d.status {
        ToolStatus::NotInstalled => (false, None, "not installed".to_string()),
        ToolStatus::Unknown { .. } => (true, None, "version unknown".to_string()),
        ToolStatus::TooOld { version, .. } => (
            true,
            Some(version.clone()),
            format!("Update {name} to continue"),
        ),
        ToolStatus::TooNew { version, .. } => (
            true,
            Some(version.clone()),
            format!("{name} {version} is newer than this companion supports: update Keo Companion to continue"),
        ),
        ToolStatus::Ready { version, .. } => (true, Some(version.clone()), String::new()),
    };
    ToolView {
        tool: d.tool.wire_name(),
        name,
        installed,
        version,
        notice,
    }
}

impl Core {
    fn status(&self) -> StatusView {
        let p = self.store.get();
        let snap = self.machine.snapshot();
        StatusView {
            state: snap.display(),
            tray: snap.tray(),
            signed_in: snap.signed_in(),
            signing_in: self.signing_in.load(Ordering::SeqCst),
            paused: snap.paused,
            start_at_login: p.start_at_login,
            first_run_done: p.first_run_done,
            approved_dirs: p
                .approved_dirs
                .iter()
                .map(|d| d.display().to_string())
                .collect(),
            tools: self
                .detections
                .lock()
                .expect("detections lock")
                .iter()
                .map(tool_view)
                .collect(),
            // The runners' latest read while signed in (Keo's baseline can lag a failed send);
            // the saved one before the first read, and after a sign-out wiped it.
            sessions: match self.registry.shown.get() {
                Some(list) if snap.signed_in() => list,
                _ => p.last_roster,
            },
            host: self.base.origin().to_string(),
            version: self.app.package_info().version.to_string(),
            error: self.last_error.lock().expect("error lock").clone(),
        }
    }

    fn emit_status(&self) {
        let _ = self.app.emit_to(PANEL, STATUS_EVENT, self.status());
    }

    fn set_error(&self, e: Option<String>) {
        *self.last_error.lock().expect("error lock") = e;
        self.emit_status();
    }

    /// Detect every tool, save the result, and move in or out of NeedsUpdate.
    async fn check_tools(&self) -> Vec<Detection> {
        let mut found = Vec::new();
        for r in &self.runners {
            found.push(r.detect().await);
        }
        for d in &found {
            tracing::info!(tool = d.tool.wire_name(), status = ?d.status, "tool check");
        }
        *self.detections.lock().expect("detections lock") = found.clone();
        if found.iter().any(Detection::needs_update) {
            self.machine.apply(Event::ToolOutOfRange);
        } else {
            self.machine.apply(Event::ToolsInRange);
        }
        self.emit_status();
        found
    }

    /// After sign-in (or at launch with a saved token): register, then listen.
    async fn register(self: &Arc<Self>) {
        // This run belongs to the sign-in it started under: after a sign-out or a newer
        // sign-in it stops without touching state (that sign-in runs its own register()).
        let generation = self.auth.generation();
        let current = || self.auth.generation() == generation;
        let mut backoff = doorbell::Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
        let detections = self.check_tools().await;
        if !current() {
            return;
        }
        // Retry until Keo confirms, whatever the tool checks do meanwhile; stop only when the
        // person signs out.
        while self.machine.snapshot().needs_registration() {
            match self
                .registry
                .register_machine(generation, &detections)
                .await
            {
                Ok(()) => {
                    self.machine.apply(Event::Registered);
                    self.set_error(None);
                    break;
                }
                Err(keo::KeoError::SignedOut) => {
                    self.machine.apply(Event::Unauthorized);
                    return;
                }
                // Another sign-in owns the machine now; its own register() runs.
                Err(keo::KeoError::Superseded) => return,
                // The machine id is signed in to another workspace: retrying cannot help. Signing
                // out wipes state.json, so the next sign-in brings a new machine id.
                Err(keo::KeoError::Tool(keo::contract::ErrorCode::NotYours)) => {
                    tracing::warn!("registration refused: machine id belongs to another workspace");
                    self.set_error(Some(
                        "This Mac is linked to another Keo workspace. Sign out, then sign in again."
                            .into(),
                    ));
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "registration failed; retrying");
                    self.set_error(Some(registration_error_text(&e).into()));
                    let wait = backoff.next_delay(&mut rand::thread_rng());
                    tokio::time::sleep(wait).await;
                    if !current() {
                        return;
                    }
                }
            }
        }
        if !current() {
            return;
        }
        if detections.iter().any(Detection::needs_update) {
            self.machine.apply(Event::ToolOutOfRange);
        }
        if let Err(e) = self.registry.sync(generation).await {
            tracing::warn!(error = %e, "roster sync failed");
        }
    }

    async fn sign_in(self: Arc<Self>) {
        // A sign-in still waiting for its browser callback (a closed tab, the wrong account)
        // gives way to this one at once instead of holding the app for 10 minutes. Of several
        // clicks queued behind it, only the newest runs.
        let attempt = self.sign_in_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        self.auth.cancel_sign_in();
        let lifecycle = self.lifecycle.lock().await;
        if self.sign_in_attempts.load(Ordering::SeqCst) != attempt {
            return;
        }
        self.signing_in.store(true, Ordering::SeqCst);
        self.emit_status();
        self.set_error(None);
        let app = self.app.clone();
        let result = self
            .auth
            .sign_in(move |url| {
                app.opener()
                    .open_url(url, None::<&str>)
                    .map_err(|e| e.to_string())
            })
            .await;
        self.signing_in.store(false, Ordering::SeqCst);
        match result {
            Ok(()) => {
                tracing::info!("signed in");
                // A new sign-in may be another account (after a revoked grant, say): its task
                // and prompt ids start a fresh ledger, and anything from before is fenced off.
                self.inbox.forget();
                // Under the settings lock: a Start at login toggle lands before (and is kept)
                // or after this, never halfway.
                let settings = self.settings.lock().expect("settings lock");
                let kept = self.store.get();
                let first = !kept.first_run_done;
                if first && kept.start_at_login_chosen {
                    // Set by hand before this first sign-in: that choice stands as it is.
                    if let Err(e) = self.store.update(|p| p.first_run_done = true) {
                        tracing::warn!(error = %e, "first run not saved");
                    }
                } else if first {
                    // First run completes at the first sign-in: start at login is on by default.
                    // The saved value is what the system actually did; a save that fails undoes
                    // the login item and leaves first run to try again at the next sign-in.
                    let launcher = self.app.autolaunch();
                    let before = launcher.is_enabled().ok();
                    let enabled = match launcher.enable() {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(error = %e, "start at login not enabled");
                            false
                        }
                    };
                    let saved = self.store.update(|p| {
                        p.first_run_done = true;
                        p.start_at_login = enabled;
                    });
                    if let Err(e) = saved {
                        tracing::warn!(error = %e, "first run not saved");
                        // Put back exactly what the system did before; unknown means leave it.
                        let undo = match before {
                            Some(false) if enabled => launcher.disable(),
                            _ => Ok(()),
                        };
                        if let Err(u) = undo {
                            tracing::warn!(error = %u, "start at login not put back");
                        }
                    }
                }
                drop(settings);
                self.machine.apply(Event::SignedIn);
                drop(lifecycle);
                self.register().await;
            }
            // Cancelled by a newer sign-in or a sign-out, which sets the state itself.
            Err(auth::AuthError::Superseded) => drop(lifecycle),
            Err(e) => {
                tracing::warn!(error = %e, "sign-in failed");
                self.machine.apply(Event::SignInFailed);
                self.set_error(Some(format!("Sign-in failed: {e}")));
                drop(lifecycle);
            }
        }
        self.emit_status();
    }

    async fn sign_out(&self) {
        // Task and prompt ids are per account: fence off every handler and ack in flight before
        // anything awaits, so none of them can act or write for the next account.
        self.inbox.forget();
        // A pending browser sign-in ends now rather than when its callback times out.
        self.auth.cancel_sign_in();
        let _lifecycle = self.lifecycle.lock().await;
        let removed = self.auth.sign_out().await;
        self.keo.reset().await;
        let settings = self.settings.lock().expect("settings lock");
        // The saved choices are gone with state.json, so start at login goes too.
        let unlaunched = self.app.autolaunch().disable();
        if let Err(e) = &unlaunched {
            tracing::warn!(error = %e, "start at login not disabled");
        }
        // Ends any sign-in begun during the awaits above, so a write checked against it under the
        // store lock never lands after the wipe.
        self.auth.end_generation();
        // Wipes state.json and resets the live pause flag and state to match it.
        let forgot = state::forget_all(&self.store, &self.machine);
        if let Err(e) = &forgot {
            tracing::warn!(error = %e, "state.json not deleted");
        }
        // Anything begun during this sign-out's awaits is fenced off; from here on the app reads
        // as signed out, so nothing new starts.
        self.auth.end_generation();
        drop(settings);
        // After the fence: a sync of the old sign-in that is still reading cannot refill it.
        self.registry.shown.clear();
        tracing::info!("signed out");
        // Anything sign-out could not undo is shown, never reported as a clean sign-out.
        // A clean sign-out also clears an error left from before (a failed registration, say).
        let text = sign_out_error_text(removed.is_err(), forgot.is_err(), unlaunched.is_err());
        self.set_error(text);
    }
}

type CoreState<'a> = tauri::State<'a, Arc<Core>>;

#[tauri::command]
fn get_status(core: CoreState<'_>) -> StatusView {
    core.status()
}

#[tauri::command]
fn sign_in(core: CoreState<'_>) {
    let core = Arc::clone(&core);
    tauri::async_runtime::spawn(core.sign_in());
}

#[tauri::command]
async fn sign_out(core: CoreState<'_>) -> Result<(), String> {
    core.sign_out().await;
    Ok(())
}

#[tauri::command]
fn set_paused(core: CoreState<'_>, paused: bool) -> Result<StatusView, String> {
    let settings = core.settings.lock().expect("settings lock");
    core.store
        .update(|p| p.paused = paused)
        .map_err(|e| e.to_string())?;
    // Saved first; the live flag changes only once the choice is on disk.
    core.machine.set_paused(paused);
    drop(settings);
    core.emit_status();
    Ok(core.status())
}

#[tauri::command]
fn set_start_at_login(core: CoreState<'_>, enabled: bool) -> Result<StatusView, String> {
    let settings = core.settings.lock().expect("settings lock");
    let launcher = core.app.autolaunch();
    // What the system did before this call, so a failed save puts back exactly that.
    let before = launcher.is_enabled().ok();
    let r = if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    };
    r.map_err(|e| e.to_string())?;
    // The login item and state.json change together: a save that fails puts the login item
    // back, so the panel never shows the opposite of what the system does.
    let saved = core.store.update(|p| {
        p.start_at_login = enabled;
        p.start_at_login_chosen = true;
    });
    if let Err(e) = saved {
        let undo = match before {
            Some(true) => launcher.enable(),
            Some(false) => launcher.disable(),
            None => Ok(()),
        };
        if let Err(u) = undo {
            tracing::warn!(error = %u, "start at login not put back");
        }
        return Err(e.to_string());
    }
    drop(settings);
    core.emit_status();
    Ok(core.status())
}

#[tauri::command]
fn add_dir(core: CoreState<'_>, path: String) -> Result<StatusView, String> {
    let dir = dirs::normalize(&path).map_err(|e| e.to_string())?;
    let settings = core.settings.lock().expect("settings lock");
    core.store
        .update(|p| dirs::add(&mut p.approved_dirs, dir))
        .map_err(|e| e.to_string())?;
    drop(settings);
    reregister(&core);
    Ok(core.status())
}

#[tauri::command]
fn remove_dir(core: CoreState<'_>, path: String) -> Result<StatusView, String> {
    let settings = core.settings.lock().expect("settings lock");
    core.store
        .update(|p| dirs::remove(&mut p.approved_dirs, std::path::Path::new(&path)))
        .map_err(|e| e.to_string())?;
    drop(settings);
    reregister(&core);
    Ok(core.status())
}

/// Send the new folder list to Keo when signed in, retrying until Keo confirms. A newer edit
/// takes over from an older one still retrying, and a sign-out or new sign-in ends it.
fn reregister(core: &Arc<Core>) {
    core.emit_status();
    if !core.machine.get().signed_in() {
        return;
    }
    let edit = core.folder_edits.fetch_add(1, Ordering::SeqCst) + 1;
    let generation = core.auth.generation();
    let core = Arc::clone(core);
    tauri::async_runtime::spawn(async move {
        let mut backoff = doorbell::Backoff::new(Duration::from_secs(5), Duration::from_secs(300));
        let current = || {
            core.folder_edits.load(Ordering::SeqCst) == edit && core.auth.generation() == generation
        };
        while current() {
            let detections = core.detections.lock().expect("detections lock").clone();
            match core
                .registry
                .register_machine(generation, &detections)
                .await
            {
                Ok(()) => return,
                Err(
                    keo::KeoError::SignedOut
                    | keo::KeoError::Superseded
                    | keo::KeoError::Tool(keo::contract::ErrorCode::NotYours),
                ) => return,
                Err(e) => {
                    tracing::warn!(error = %e, "folder update not sent; retrying");
                    let wait = backoff.next_delay(&mut rand::thread_rng());
                    tokio::time::sleep(wait).await;
                }
            }
        }
    });
}

/// Stop a browser sign-in still waiting for its callback.
#[tauri::command]
fn cancel_sign_in(core: CoreState<'_>) {
    core.auth.cancel_sign_in();
}

/// Quit the companion (the panel's Quit button; there is no main window to close).
#[tauri::command]
fn quit(app: tauri::AppHandle) {
    tracing::info!("quit");
    app.exit(0);
}

#[tauri::command]
fn log_tail(core: CoreState<'_>, lines: Option<usize>) -> Vec<String> {
    log::tail(&core.log_dir, lines.unwrap_or(100).min(1000))
}

/// Open a page on Keo's own host in the browser. Other URLs are refused.
#[tauri::command]
fn open_url(core: CoreState<'_>, url: String) -> Result<(), String> {
    if !core.base.owns(&url) {
        return Err("only Keo pages open from here".into());
    }
    core.app
        .opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// What the panel says after a sign-out that could not undo everything, with what to do about
/// each part. `None` for a clean sign-out.
fn sign_out_error_text(keychain: bool, state: bool, login_item: bool) -> Option<String> {
    let mut parts = Vec::new();
    // The next start deletes a leftover sign-in only once state.json is gone.
    if keychain && !state {
        parts.push(
            "The saved sign-in could not be deleted from your keychain; Keo Companion deletes it \
             when it next starts.",
        );
    }
    if keychain && state {
        parts.push(
            "The saved sign-in could not be deleted; delete \"Keo Companion\" in Keychain Access \
             (macOS) or Credential Manager (Windows).",
        );
    }
    if state {
        parts.push(
            "This machine's saved settings (state.json) could not be deleted; check that \
             folder's permissions, then sign in and out again.",
        );
    }
    if login_item {
        parts.push(
            "Start at login could not be turned off; turn it off in Login Items (macOS System \
             Settings > General) or Startup apps (Windows Task Manager).",
        );
    }
    (!parts.is_empty()).then(|| format!("Signed out, but: {}", parts.join(" ")))
}

/// What the panel says while registration retries. "Needs an update" only when Keo answered and
/// refused the call (the server lacks what this app needs); anything else, a token refresh that
/// failed offline included, is "Could not reach Keo".
fn registration_error_text(e: &keo::KeoError) -> &'static str {
    use keo::contract::ErrorCode as C;
    use keo::KeoError as E;
    match e {
        E::RateLimited | E::Status(429) | E::Tool(C::KeoUnavailable) => "Keo is busy; retrying.",
        E::Rpc(_) | E::Tool(C::InvalidArgument | C::NotFound | C::NotYours | C::Other) => {
            "Keo needs an update before this app can register. Retrying."
        }
        E::Status(s) if (400..500).contains(s) && *s != 401 => {
            "Keo needs an update before this app can register. Retrying."
        }
        _ => "Could not reach Keo; retrying.",
    }
}

// ---- tray and panel ----

/// The Keo K for each state: black template PNGs on macOS (the menu bar tints them), the
/// colour K as .ico on Windows. Regenerated by icons/tray/build.py.
#[cfg(not(windows))]
fn tray_bytes(t: TrayState) -> &'static [u8] {
    match t {
        TrayState::SignedOut => include_bytes!("../icons/tray/signed-out.png"),
        TrayState::Listening => include_bytes!("../icons/tray/listening.png"),
        TrayState::Paused => include_bytes!("../icons/tray/paused.png"),
        TrayState::Working => include_bytes!("../icons/tray/working.png"),
        TrayState::NeedsAttention => include_bytes!("../icons/tray/needs-attention.png"),
        TrayState::NeedsUpdate => include_bytes!("../icons/tray/needs-update.png"),
    }
}

#[cfg(windows)]
fn tray_bytes(t: TrayState) -> &'static [u8] {
    match t {
        TrayState::SignedOut => include_bytes!("../icons/tray/win/signed-out.ico"),
        TrayState::Listening => include_bytes!("../icons/tray/win/listening.ico"),
        TrayState::Paused => include_bytes!("../icons/tray/win/paused.ico"),
        TrayState::Working => include_bytes!("../icons/tray/win/working.ico"),
        TrayState::NeedsAttention => include_bytes!("../icons/tray/win/needs-attention.ico"),
        TrayState::NeedsUpdate => include_bytes!("../icons/tray/win/needs-update.ico"),
    }
}

/// Template images are a macOS idea: the colour K is the Windows look.
const TRAY_TEMPLATE: bool = cfg!(target_os = "macos");

fn tray_image(t: TrayState) -> Option<Image<'static>> {
    Image::from_bytes(tray_bytes(t)).ok()
}

fn apply_tray(tray: &TrayIcon, t: TrayState) {
    let _ = tray.set_icon(tray_image(t));
    let _ = tray.set_icon_as_template(TRAY_TEMPLATE);
    let _ = tray.set_tooltip(Some(t.tooltip()));
}

fn panel(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    if let Some(w) = app.get_webview_window(PANEL) {
        return Ok(w);
    }
    let w = WebviewWindowBuilder::new(app, PANEL, WebviewUrl::App("index.html".into()))
        .title("Keo Companion")
        .inner_size(PANEL_W, PANEL_H)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    let handle = w.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            let _ = handle.hide();
        }
    });
    Ok(w)
}

/// Gap between the panel and the edges of the screen's visible area, in logical pixels.
const PANEL_MARGIN: f64 = 8.0;

/// A rectangle: x, y, width, height, in whatever unit its caller uses for all of them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Where the panel goes and how tall it is, in the unit of its inputs (all one unit). The panel
/// is centred on `anchor` (the cursor at the tray click, which is on the icon; the top right with
/// no anchor), inside the visible area of the screen (the part not under the menu bar, the Dock or
/// the taskbar), `margin` from the sides. With the anchor in the top half (the macOS menu bar, a
/// top taskbar) it sits flush under the bar at the visible top; in the bottom half (the usual
/// Windows taskbar) it sits flush above the bar. It gets shorter when the screen is too short,
/// `margin` from the far edge, and the page scrolls.
fn panel_frame(
    anchor: Option<(f64, f64)>,
    visible: Area,
    width: f64,
    height: f64,
    margin: f64,
) -> Area {
    let left = visible.x + margin;
    let right = visible.x + visible.w - margin;
    let top = visible.y;
    let bottom = visible.y + visible.h;
    let x = anchor.map_or(right - width, |(x, _)| x - width / 2.0);
    let x = x.min(right - width).max(left);
    let above = anchor.is_some_and(|(_, y)| y > visible.y + visible.h / 2.0);
    let (y, h) = if above {
        let h = height.min(bottom - top - margin).max(0.0);
        (bottom - h, h)
    } else {
        (top, height.min(bottom - margin - top).max(0.0))
    };
    Area { x, y, w: width, h }
}

/// One screen as the platform reports it: whole bounds and visible area (`work_area`) in that
/// screen's physical pixels, and its scale factor.
#[derive(Clone, Copy, Debug)]
struct Screen {
    bounds: Area,
    visible: Area,
    scale: f64,
}

impl Screen {
    /// The screen's bounds and visible area in the shared space: points on macOS (pixels over
    /// the screen's own scale), physical pixels on Windows.
    fn shared(&self, points: bool) -> (Area, Area) {
        let by = if points { self.scale } else { 1.0 };
        (scaled(self.bounds, by), scaled(self.visible, by))
    }
}

fn scaled(a: Area, by: f64) -> Area {
    Area {
        x: a.x / by,
        y: a.y / by,
        w: a.w / by,
        h: a.h / by,
    }
}

/// A tray click: the icon's rect as the tray reports it (logged; a fallback anchor on Windows
/// only, as on macOS its scale and y origin are unknown) and the cursor at that moment in the shared screen space
/// (see [`cursor_point`]).
#[derive(Clone, Copy, Debug)]
struct TrayClick {
    rect: Rect,
    cursor: Option<(f64, f64)>,
}

/// Screens share one coordinate space: points on macOS, physical pixels on Windows.
const SHARED_SPACE_IS_POINTS: bool = cfg!(target_os = "macos");

/// The cursor in the shared space. On macOS tao reports it in points times the primary screen's
/// scale, a known scale, so it is exact on every screen.
fn cursor_point(app: &AppHandle) -> Option<(f64, f64)> {
    let p = app.cursor_position().ok()?;
    let by = if SHARED_SPACE_IS_POINTS {
        app.primary_monitor().ok().flatten()?.scale_factor()
    } else {
        1.0
    };
    Some((p.x / by, p.y / by))
}

fn holds(a: Area, x: f64, y: f64) -> bool {
    x >= a.x && x < a.x + a.w && y >= a.y && y < a.y + a.h
}

/// Where the panel goes, in the shared space: the screen under the cursor at the click, then
/// [`panel_frame`] on its visible area. Without a cursor on a screen, `fallback` (the tray rect's
/// centre, given only where the rect is in the shared space: Windows) is the anchor; with
/// neither, the first screen's (the primary's) top right. The panel's size and margin are
/// points, so on Windows (physical pixels) they are scaled by the chosen screen's scale. Returns
/// the frame and the chosen screen's shared bounds, both in the shared space.
fn place(
    cursor: Option<(f64, f64)>,
    fallback: Option<(f64, f64)>,
    screens: &[Screen],
    points: bool,
) -> Option<(Area, Area)> {
    let on_a_screen = |(x, y): (f64, f64)| {
        let s = screens.iter().find(|s| holds(s.shared(points).0, x, y))?;
        Some((s, Some((x, y))))
    };
    let (screen, anchor) = cursor
        .and_then(on_a_screen)
        .or_else(|| fallback.and_then(on_a_screen))
        .or_else(|| screens.first().map(|s| (s, None)))?;
    let (bounds, visible) = screen.shared(points);
    let unit = if points { 1.0 } else { screen.scale };
    let f = panel_frame(
        anchor,
        visible,
        PANEL_W * unit,
        PANEL_H * unit,
        PANEL_MARGIN * unit,
    );
    Some((f, bounds))
}

/// Show the panel under the tray icon (or at the top right when the click is unknown).
fn show_panel(app: &AppHandle) {
    let w = match panel(app) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "panel not created");
            return;
        }
    };
    let click = app
        .try_state::<Arc<Core>>()
        .and_then(|c| *c.tray_rect.lock().expect("tray lock"));
    // The primary first, so the fallback is the primary's top right; a duplicate is harmless.
    let mut monitors: Vec<_> = app.primary_monitor().ok().flatten().into_iter().collect();
    monitors.extend(app.available_monitors().unwrap_or_default());
    let screens: Vec<Screen> = monitors
        .iter()
        .map(|m| {
            let (p, z, wa) = (m.position(), m.size(), m.work_area());
            Screen {
                bounds: Area {
                    x: p.x as f64,
                    y: p.y as f64,
                    w: z.width as f64,
                    h: z.height as f64,
                },
                visible: Area {
                    x: wa.position.x as f64,
                    y: wa.position.y as f64,
                    w: wa.size.width as f64,
                    h: wa.size.height as f64,
                },
                scale: m.scale_factor(),
            }
        })
        .collect();
    let cursor = click.and_then(|c| c.cursor);
    // On macOS the rect's scale and y origin are unknown, so it is never an anchor there.
    let fallback = click.filter(|_| !SHARED_SPACE_IS_POINTS).map(|c| {
        let (p, z) = (
            c.rect.position.to_physical::<f64>(1.0),
            c.rect.size.to_physical::<f64>(1.0),
        );
        (p.x + z.width / 2.0, p.y + z.height / 2.0)
    });
    let placed = place(cursor, fallback, &screens, SHARED_SPACE_IS_POINTS);
    // One line per click, so a misplaced panel comes with its numbers.
    tracing::info!(
        rect = ?click.map(|c| c.rect),
        cursor = ?cursor,
        screen = ?placed.map(|(_, b)| b),
        frame = ?placed.map(|(f, _)| f),
        "panel placed"
    );
    if let Some((f, _)) = placed {
        // The frame is in the shared space: points on macOS, physical pixels on Windows.
        if SHARED_SPACE_IS_POINTS {
            let _ = w.set_size(LogicalSize::new(f.w, f.h));
            let _ = w.set_position(LogicalPosition::new(f.x, f.y));
        } else {
            // Move first: a move to a screen of another scale rescales the window.
            let _ = w.set_position(PhysicalPosition::new(
                f.x.round() as i32,
                f.y.round() as i32,
            ));
            let _ = w.set_size(PhysicalSize::new(f.w.round() as u32, f.h.round() as u32));
        }
    }
    let _ = w.show();
    let _ = w.set_focus();
    if let Some(core) = app.try_state::<Arc<Core>>() {
        core.emit_status();
    }
}

fn toggle_panel(app: &AppHandle) {
    match app.get_webview_window(PANEL) {
        Some(w) if w.is_visible().unwrap_or(false) => {
            let _ = w.hide();
        }
        _ => show_panel(app),
    }
}

// ---- setup ----

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    let handle = app.handle().clone();
    let data_dir = handle.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    let log_dir = log::log_dir(&data_dir);
    if let Some(guard) = log::init(&log_dir) {
        app.manage(LogGuard(Mutex::new(Some(guard))));
    }

    let config = handle.config();
    let identifier = config.identifier.clone();
    let base = BaseUrl::from_plugin_config(config.plugins.0.get("keo"));
    tracing::info!(
        version = %handle.package_info().version,
        identifier = %identifier,
        host = base.origin(),
        "starting"
    );

    let http = reqwest::Client::builder()
        .user_agent(format!("keo-companion/{}", handle.package_info().version))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let store = Arc::new(Store::load(&data_dir));
    let persisted = store.get();
    let machine = Arc::new(Machine::new(
        if persisted.first_run_done {
            AppState::SignedOut
        } else {
            AppState::FirstRun
        },
        persisted.paused,
    ));
    let auth = Arc::new(Auth::new(
        base.clone(),
        http.clone(),
        store.clone(),
        &identifier,
    ));
    let keo = Arc::new(KeoClient::new(
        base.clone(),
        http.clone(),
        auth.clone(),
        handle.package_info().version.to_string(),
    ));
    runner::codex::set_data_dir(data_dir.clone());
    let runners = runner::all(runner::SessionPolicy::for_keo(&base));
    let registry = Arc::new(Registry::new(keo.clone(), store.clone(), runners.clone()));
    let relay = Arc::new(Relay::new(
        keo.clone(),
        store.clone(),
        machine.clone(),
        runners.clone(),
        registry.clone(),
    ));
    let inbox = Arc::new(inbox::Inbox::new(
        keo.clone(),
        store.clone(),
        machine.clone(),
        relay.clone(),
        registry.clone(),
    ));
    let core = Arc::new(Core {
        app: handle.clone(),
        base: base.clone(),
        store: store.clone(),
        machine: machine.clone(),
        auth: auth.clone(),
        keo,
        registry,
        inbox: inbox.clone(),
        detections: Mutex::new(Vec::new()),
        runners,
        log_dir,
        signing_in: AtomicBool::new(false),
        sign_in_attempts: std::sync::atomic::AtomicU64::new(0),
        lifecycle: tokio::sync::Mutex::new(()),
        settings: Mutex::new(()),
        folder_edits: std::sync::atomic::AtomicU64::new(0),
        last_error: Mutex::new(None),
        tray_rect: Mutex::new(None),
    });
    app.manage(core.clone());

    // Tray.
    let initial = machine.snapshot().tray();
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(initial.tooltip())
        .icon_as_template(TRAY_TEMPLATE)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(core) = app.try_state::<Arc<Core>>() {
                    let cursor = cursor_point(app);
                    *core.tray_rect.lock().expect("tray lock") = Some(TrayClick { rect, cursor });
                }
                toggle_panel(app);
            }
        });
    if let Some(img) = tray_image(initial) {
        tray = tray.icon(img);
    }
    let tray = tray.build(app)?;

    // State watcher: tray icon + panel status on every change.
    {
        let core = core.clone();
        let mut rx = machine.subscribe();
        tauri::async_runtime::spawn(async move {
            loop {
                let s = *rx.borrow_and_update();
                apply_tray(&tray, s.tray());
                core.emit_status();
                if rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    // Roster watcher: the panel's session list on every change the runners report (a state
    // change in a session moves no machine state, so the watcher above would miss it).
    {
        let core = core.clone();
        let mut rx = core.registry.shown.subscribe();
        tauri::async_runtime::spawn(async move {
            while rx.changed().await.is_ok() {
                core.emit_status();
            }
        });
    }

    // Doorbell -> inbox -> relay.
    let (ring_tx, ring_rx) = mpsc::channel(16);
    tauri::async_runtime::spawn(doorbell::run(
        base,
        http,
        store.clone(),
        machine.subscribe(),
        ring_tx,
    ));
    tauri::async_runtime::spawn(inbox.run(ring_rx));
    relay.spawn_listeners();

    // Startup: tools, then sign in from the saved token (only after the first run).
    {
        let core = core.clone();
        tauri::async_runtime::spawn(async move {
            let generation = core.auth.generation();
            core.check_tools().await;
            // Restoring the saved sign-in is a lifecycle step like any other: it waits for a
            // browser sign-in or sign-out under way, and is dropped if one ran meanwhile.
            let lifecycle = core.lifecycle.lock().await;
            if core.auth.generation() == generation {
                core.auth.clear_leftover();
            }
            let restore = core.auth.generation() == generation
                && core.store.get().first_run_done
                && core.auth.has_refresh_token();
            if restore {
                core.machine.apply(Event::SignedIn);
                drop(lifecycle);
                core.register().await;
            } else if core.auth.generation() != generation {
                // A sign-in or sign-out ran during the tool checks: it owns the state now.
            } else if !core.store.get().first_run_done {
                // First run: show the panel so the person can sign in.
                let app = core.app.clone();
                let _ = core.app.run_on_main_thread(move || show_panel(&app));
            }
        });
    }

    // Hourly, as two independent loops: a slow or hanging update check can never hold up the
    // tool-version check.
    {
        let app = core.app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                check_for_update(&app).await;
                tokio::time::sleep(HOURLY).await;
            }
        });
    }
    {
        let core = core.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(HOURLY).await;
                core.check_tools().await;
            }
        });
    }
    Ok(())
}

/// The whole update check (request plus parsing) never takes longer than this.
const UPDATE_CHECK_LIMIT: Duration = Duration::from_secs(2 * keo::HTTP_TIMEOUT.as_secs());

/// Run `fut` for at most `limit`; `None` when it ran out of time.
async fn bounded<T>(limit: Duration, fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(limit, fut).await.ok()
}

/// The update check, bounded twice: the updater's own request timeout, and an outer limit on
/// the whole check. The signing key is a placeholder until release signing exists, so errors
/// are only logged.
async fn check_for_update(app: &AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let updater = match app.updater_builder().timeout(keo::HTTP_TIMEOUT).build() {
        Ok(u) => u,
        Err(e) => {
            tracing::info!(error = %e, "updater unavailable");
            return;
        }
    };
    match bounded(UPDATE_CHECK_LIMIT, updater.check()).await {
        Some(Ok(Some(update))) => tracing::info!(version = %update.version, "update available"),
        Some(Ok(None)) => tracing::info!("no update"),
        Some(Err(e)) => tracing::info!(error = %e, "update check failed"),
        None => tracing::info!("update check timed out"),
    }
}

/// Keeps the log writer alive for the life of the app.
struct LogGuard(#[allow(dead_code)] Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first: a second launch shows the running app's panel and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_panel(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            quit,
            cancel_sign_in,
            get_status,
            sign_in,
            sign_out,
            set_paused,
            set_start_at_login,
            add_dir,
            remove_dir,
            log_tail,
            open_url
        ])
        .setup(setup)
        .build(tauri::generate_context!())
        .expect("error while building Keo Companion")
        .run(|_app, event| {
            // A menu-bar app keeps running with no window open; Quit in the panel exits with a
            // code (`quit`), which is never prevented.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(x: f64, y: f64, w: f64, h: f64) -> Area {
        Area { x, y, w, h }
    }

    /// The main display: a 5K at 2x (2560x1440 pt) with a 24 pt menu bar, origin (0, 0).
    fn main_5k() -> Screen {
        Screen {
            bounds: area(0.0, 0.0, 5120.0, 2880.0),
            visible: area(0.0, 48.0, 5120.0, 2832.0),
            scale: 2.0,
        }
    }

    /// Three displays: the 5K main one, a 4K at 2x (1920x1080 pt) to its right, and the built-in
    /// display at 2x (1728x1117 pt) below it, each with its own menu bar.
    fn three_screens() -> [Screen; 3] {
        let four_k = Screen {
            bounds: area(5120.0, 0.0, 3840.0, 2160.0),
            visible: area(5120.0, 48.0, 3840.0, 2112.0),
            scale: 2.0,
        };
        let built_in = Screen {
            bounds: area(800.0, 2880.0, 3456.0, 2234.0),
            visible: area(800.0, 2928.0, 3456.0, 2186.0),
            scale: 2.0,
        };
        [main_5k(), four_k, built_in]
    }

    #[test]
    fn place_opens_under_the_cursor_on_the_main_display() {
        let (f, b) = place(Some((1162.0, 5.0)), None, &three_screens(), true).unwrap();
        assert_eq!(b, area(0.0, 0.0, 2560.0, 1440.0));
        assert_eq!(f, area(1162.0 - PANEL_W / 2.0, 24.0, PANEL_W, PANEL_H));
    }

    #[test]
    fn place_clamps_near_the_right_edge() {
        let (f, _) = place(Some((2550.0, 5.0)), None, &three_screens(), true).unwrap();
        assert_eq!((f.x, f.y), (2560.0 - PANEL_MARGIN - PANEL_W, 24.0));
        let (f, _) = place(Some((3.0, 5.0)), None, &three_screens(), true).unwrap();
        assert_eq!(f.x, PANEL_MARGIN);
    }

    #[test]
    fn place_uses_the_visible_top_of_the_cursors_screen() {
        // The cursor on the 4K's menu bar: that screen's visible top, centred on the cursor.
        let (f, b) = place(Some((3500.0, 6.0)), None, &three_screens(), true).unwrap();
        assert_eq!(b, area(2560.0, 0.0, 1920.0, 1080.0));
        assert_eq!((f.x, f.y), (3500.0 - PANEL_W / 2.0, 24.0));
        // The built-in display below: its own visible top, not the main display's.
        let (f, b) = place(Some((1000.0, 1445.0)), None, &three_screens(), true).unwrap();
        assert_eq!(b, area(400.0, 1440.0, 1728.0, 1117.0));
        assert_eq!((f.x, f.y), (1000.0 - PANEL_W / 2.0, 1464.0));
    }

    #[test]
    fn place_falls_back_to_the_first_screen() {
        // No cursor, or one on no screen, and no fallback (macOS): the first screen's top right.
        let top_right = (2560.0 - PANEL_MARGIN - PANEL_W, 24.0);
        let (f, _) = place(None, None, &three_screens(), true).unwrap();
        assert_eq!((f.x, f.y), top_right);
        let (f, _) = place(Some((-50.0, -50.0)), None, &three_screens(), true).unwrap();
        assert_eq!((f.x, f.y), top_right);
        // A fallback on no screen: the same.
        let (f, _) = place(None, Some((9000.0, 5.0)), &three_screens(), true).unwrap();
        assert_eq!((f.x, f.y), top_right);
        // No screens: nothing to place.
        assert!(place(Some((1.0, 1.0)), None, &[], true).is_none());
    }

    #[test]
    fn place_on_windows_opens_above_a_bottom_taskbar() {
        // Windows: one physical space; a 1.5x laptop at the left, a 1x screen to its right with a
        // 48 px taskbar at the bottom, the cursor on its tray icon.
        let laptop = Screen {
            bounds: area(0.0, 0.0, 2880.0, 1800.0),
            visible: area(0.0, 0.0, 2880.0, 1728.0),
            scale: 1.5,
        };
        let right = Screen {
            bounds: area(2880.0, 0.0, 1920.0, 1080.0),
            visible: area(2880.0, 0.0, 1920.0, 1032.0),
            scale: 1.0,
        };
        let (f, b) = place(Some((4710.0, 1050.0)), None, &[laptop, right], false).unwrap();
        assert_eq!(b, right.bounds);
        assert_eq!(
            f,
            area(
                4800.0 - PANEL_MARGIN - PANEL_W,
                1032.0 - PANEL_H,
                PANEL_W,
                PANEL_H
            )
        );
        // On the 1.5x laptop, above its taskbar: the frame is in its pixels, 1.5x the points.
        let (f, b) = place(Some((1400.0, 1760.0)), None, &[laptop, right], false).unwrap();
        assert_eq!(b, laptop.bounds);
        let (w, h) = (PANEL_W * 1.5, PANEL_H * 1.5);
        assert_eq!(f, area(1400.0 - w / 2.0, 1728.0 - h, w, h));
        // No usable cursor: the tray rect's centre, in the same pixels, still opens it above
        // the bottom taskbar; a top taskbar opens it under.
        let (g, _) = place(None, Some((4710.0, 1050.0)), &[laptop, right], false).unwrap();
        assert_eq!(g.y, 1032.0 - PANEL_H);
        let top_bar = Screen {
            bounds: area(0.0, 0.0, 1920.0, 1080.0),
            visible: area(0.0, 48.0, 1920.0, 1032.0),
            scale: 1.0,
        };
        let (g, _) = place(Some((-1.0, -1.0)), Some((1800.0, 20.0)), &[top_bar], false).unwrap();
        assert_eq!((g.x, g.y), (1920.0 - PANEL_MARGIN - PANEL_W, 48.0));
    }

    #[test]
    fn panel_frame_shrinks_on_short_screens() {
        // Under a top bar: the panel ends `margin` above the bottom.
        let short = area(0.0, 24.0, 1920.0, 300.0);
        let f = panel_frame(Some((900.0, 5.0)), short, 380.0, 560.0, 8.0);
        assert_eq!((f.y, f.h), (24.0, 292.0));
        // Above a bottom taskbar: the panel ends at the bar and stays `margin` from the top.
        let short = area(0.0, 0.0, 1920.0, 400.0);
        let f = panel_frame(Some((1800.0, 420.0)), short, 380.0, 560.0, 8.0);
        assert_eq!((f.y, f.h), (8.0, 392.0));
    }

    #[test]
    fn sign_out_text_names_each_part_left_behind() {
        assert_eq!(sign_out_error_text(false, false, false), None);
        let all = sign_out_error_text(true, true, true).unwrap();
        assert!(
            all.contains("Keychain Access")
                && all.contains("state.json")
                && all.contains("Login Items")
        );
        assert!(
            !all.contains("next starts"),
            "no promise the next start cannot keep"
        );
        assert!(sign_out_error_text(true, false, false)
            .unwrap()
            .contains("next starts"));
        let one = sign_out_error_text(false, false, true).unwrap();
        assert!(one.contains("Startup apps") && !one.contains("keychain"));
    }

    #[test]
    fn registration_text_says_unreachable_only_when_no_answer_came() {
        use keo::contract::ErrorCode;
        use keo::KeoError as E;
        let unreachable = "Could not reach Keo; retrying.";
        assert_eq!(
            registration_error_text(&E::Network("dns".into())),
            unreachable
        );
        assert_eq!(registration_error_text(&E::Status(503)), unreachable);
        // A saved sign-in refreshing its token offline: Keo never answered.
        let offline: E = auth::AuthError::Network("dns".into()).into();
        assert_eq!(registration_error_text(&offline), unreachable);
        assert_eq!(
            registration_error_text(&E::Protocol("html".into())),
            unreachable
        );
        let update = "Keo needs an update before this app can register. Retrying.";
        assert_eq!(
            registration_error_text(&E::Tool(ErrorCode::InvalidArgument)),
            update
        );
        assert_eq!(registration_error_text(&E::Rpc(-32601)), update);
        assert_eq!(registration_error_text(&E::Status(400)), update);
        assert_eq!(
            registration_error_text(&E::RateLimited),
            "Keo is busy; retrying."
        );
    }

    // The macOS/Linux PNG set; the Windows .ico set is checked below.
    #[cfg(not(windows))]
    #[test]
    fn every_tray_icon_decodes_to_44px_rgba() {
        for t in [
            TrayState::SignedOut,
            TrayState::Listening,
            TrayState::Paused,
            TrayState::Working,
            TrayState::NeedsAttention,
            TrayState::NeedsUpdate,
        ] {
            let img = tray_image(t).expect("tray icon decodes");
            assert_eq!((img.width(), img.height()), (44, 44));
            assert_eq!(img.rgba().len(), 44 * 44 * 4);
        }
    }

    #[test]
    fn every_windows_tray_icon_decodes() {
        for bytes in [
            &include_bytes!("../icons/tray/win/signed-out.ico")[..],
            include_bytes!("../icons/tray/win/listening.ico"),
            include_bytes!("../icons/tray/win/paused.ico"),
            include_bytes!("../icons/tray/win/working.ico"),
            include_bytes!("../icons/tray/win/needs-attention.ico"),
            include_bytes!("../icons/tray/win/needs-update.ico"),
        ] {
            let img = Image::from_bytes(bytes).expect(".ico decodes");
            assert_eq!(img.rgba().len(), (img.width() * img.height() * 4) as usize);
        }
    }

    #[tokio::test]
    async fn bounded_gives_up_on_a_hanging_future() {
        let hung = bounded(Duration::from_millis(20), std::future::pending::<()>()).await;
        assert!(hung.is_none());
        assert_eq!(bounded(Duration::from_secs(1), async { 7 }).await, Some(7));
    }

    #[test]
    fn update_check_limit_is_finite_and_short() {
        assert!(UPDATE_CHECK_LIMIT > keo::HTTP_TIMEOUT);
        assert!(UPDATE_CHECK_LIMIT <= Duration::from_secs(120));
        assert!(UPDATE_CHECK_LIMIT < HOURLY);
    }
}
