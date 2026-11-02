//! The companion's own `codex app-server`: loopback websocket, capability token. The spike
//! showed `codex app-server daemon` serves only a unix control socket, and the desktop app's
//! app-server is stdio-only, so the companion always starts its own. Every start writes a new
//! token to `codex.token` (0600), so a token is only ever sent to the server it was made for. The
//! server's pid is kept in `codex.pid`: a server an earlier launch left running is stopped, never
//! attached to, since nothing proves a listener on an old port is still that server.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rand::RngCore;

pub const TOKEN_FILE: &str = "codex.token";
pub const PID_FILE: &str = "codex.pid";
pub const LOG_FILE: &str = "codex-app-server.log";
/// How long a new server may take to answer `/readyz`.
const READY_WAIT: Duration = Duration::from_secs(20);

/// A new token for the next server (32 random bytes, hex), written with mode 0600.
pub fn new_token(dir: &Path) -> std::io::Result<(PathBuf, String)> {
    let path = dir.join(TOKEN_FILE);
    std::fs::create_dir_all(dir)?;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let t: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(t.as_bytes())?;
    restrict(&path)?;
    Ok((path, t))
}

fn restrict(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn saved_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join(PID_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&p| p > 1)
}

fn save_pid(dir: &Path, pid: u32) -> std::io::Result<()> {
    std::fs::write(dir.join(PID_FILE), pid.to_string())
}

/// Stop the app-server an earlier launch left running (with every process it started), when
/// the saved pid is still a `codex app-server`.
pub fn stop_stale(dir: &Path) {
    let Some(pid) = saved_pid(dir) else { return };
    let _ = std::fs::remove_file(dir.join(PID_FILE));
    if is_app_server(&command_of(pid)) {
        tracing::info!(pid, "codex: stopping the app-server an earlier launch left");
        kill_tree(pid);
    }
}

fn is_app_server(command: &str) -> bool {
    command.contains("codex") && command.contains("app-server")
}

/// A quiet helper command: no console window on Windows.
fn quiet(program: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The command line of a running process, or empty.
fn command_of(pid: u32) -> String {
    #[cfg(unix)]
    let out = quiet("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    #[cfg(windows)]
    let out = quiet("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine"),
        ])
        .output();
    #[cfg(not(any(unix, windows)))]
    let out: std::io::Result<std::process::Output> =
        Err(std::io::Error::other("unsupported platform"));
    out.map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// End `pid` and every process it started: the `codex` on PATH is often a Node launcher whose
/// child is the real server. On unix the server leads its own process group; on Windows
/// `taskkill /T` ends the tree.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    let _ = quiet("/bin/kill")
        .arg("-TERM")
        .arg(format!("-{pid}"))
        .stdout(std::process::Stdio::null())
        .status();
    #[cfg(windows)]
    let _ = quiet("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .status();
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

pub fn ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}")
}

/// `GET /readyz` answers 200.
pub async fn ready(port: u16) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
    else {
        return false;
    };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/readyz")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// A free loopback port, read from a socket bound to port 0.
pub fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// A started app-server. The `codex` on PATH is often a Node launcher that starts the native
/// binary as its own child, so the server runs in its own process group (unix) and dropping this ends
/// the whole tree. A companion that quits without dropping it leaves the server running; the
/// next launch stops it through `codex.pid`.
pub struct Server {
    child: tokio::process::Child,
    pub url: String,
    pub token: String,
    dir: PathBuf,
}

impl Server {
    /// The process is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            kill_tree(pid);
            if saved_pid(&self.dir) == Some(pid) {
                let _ = std::fs::remove_file(self.dir.join(PID_FILE));
            }
        }
        let _ = self.child.start_kill();
    }
}

/// Start `codex app-server` on a free port with a new token and wait for `/readyz`. Output goes
/// to the log file.
pub async fn spawn(bin: &Path, dir: &Path, log_dir: &Path) -> Result<Server, String> {
    let (token_file, token) = new_token(dir).map_err(|e| format!("codex token: {e}"))?;
    let port = free_port().map_err(|e| format!("no free port: {e}"))?;
    std::fs::create_dir_all(log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join(LOG_FILE))
        .map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new(bin);
    if let Some(p) = crate::runner::child_path(bin) {
        cmd.env("PATH", p);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd
        .arg("app-server")
        .arg("--listen")
        .arg(ws_url(port))
        .arg("--ws-auth")
        .arg("capability-token")
        .arg("--ws-token-file")
        .arg(&token_file)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start codex app-server: {e}"))?;
    if let Some(pid) = child.id() {
        if let Err(e) = save_pid(dir, pid) {
            tracing::warn!(error = %e, "codex: pid not saved");
        }
    }
    let mut server = Server {
        child,
        url: ws_url(port),
        token,
        dir: dir.to_path_buf(),
    };
    let deadline = Instant::now() + READY_WAIT;
    loop {
        if let Ok(Some(status)) = server.child.try_wait() {
            return Err(format!("codex app-server exited at start ({status})"));
        }
        if ready(port).await {
            return Ok(server);
        }
        if Instant::now() > deadline {
            drop(server);
            return Err("codex app-server did not get ready".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_is_new_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let (path, t) = new_token(dir.path()).unwrap();
        assert_eq!(t.len(), 64);
        let (_, again) = new_token(dir.path()).unwrap();
        assert_ne!(t, again);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn stale_pid_is_stopped_only_when_it_is_an_app_server() {
        assert!(is_app_server(
            "/usr/local/bin/codex app-server --listen ws://127.0.0.1:1"
        ));
        assert!(!is_app_server("/usr/bin/vim notes.txt"));
        let dir = tempfile::tempdir().unwrap();
        stop_stale(dir.path());
        save_pid(dir.path(), std::process::id()).unwrap();
        stop_stale(dir.path());
        assert_eq!(saved_pid(dir.path()), None);
        assert_eq!(ws_url(45123), "ws://127.0.0.1:45123");
    }
}
