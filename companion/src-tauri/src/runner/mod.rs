//! The runners: one per coding tool (Codex, Claude Code). The core only sees `dyn Runner` and
//! the `RunnerEvent` enum, so a runner file can grow without a change to any core file.
//!
//! To add or replace a runner: implement `Runner` in its own file and list it in `all()`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::keo::BaseUrl;
use crate::versions;

pub mod claude;
pub mod codex;
pub mod form;

/// The coding tools the companion knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKind {
    ClaudeCode,
    Codex,
}

impl ToolKind {
    /// The `agent` / `tool` value Keo's MCP server expects.
    pub fn wire_name(self) -> &'static str {
        match self {
            ToolKind::ClaudeCode => "claude-code",
            ToolKind::Codex => "codex",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "claude-code" => Some(ToolKind::ClaudeCode),
            "codex" => Some(ToolKind::Codex),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ToolKind::ClaudeCode => "Claude Code",
            ToolKind::Codex => "Codex",
        }
    }

    /// The oldest version the companion drives (`crate::versions`).
    pub fn min_version(self) -> semver::Version {
        match self {
            ToolKind::ClaudeCode => versions::CLAUDE_CODE_MIN,
            ToolKind::Codex => versions::CODEX_MIN,
        }
    }

    /// The newest version tested with this build, when there is a ceiling (`crate::versions`).
    pub fn max_tested(self) -> Option<semver::Version> {
        match self {
            ToolKind::ClaudeCode => versions::CLAUDE_CODE_MAX_TESTED,
            ToolKind::Codex => versions::CODEX_MAX_TESTED,
        }
    }

    /// The binary name searched on PATH.
    pub fn binary(self) -> &'static str {
        match self {
            ToolKind::ClaudeCode => "claude",
            ToolKind::Codex => "codex",
        }
    }
}

/// What `detect()` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ToolStatus {
    NotInstalled,
    /// Found, and the version could not be read.
    Unknown {
        path: PathBuf,
    },
    TooOld {
        path: PathBuf,
        version: String,
    },
    /// Newer than the newest version tested with this build: the companion needs an update.
    TooNew {
        path: PathBuf,
        version: String,
    },
    Ready {
        path: PathBuf,
        version: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub tool: ToolKind,
    #[serde(flatten)]
    pub status: ToolStatus,
}

impl Detection {
    pub fn needs_update(&self) -> bool {
        matches!(
            self.status,
            ToolStatus::TooOld { .. } | ToolStatus::TooNew { .. }
        )
    }
}

/// One coding session on this machine, as Keo's `Register` event describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    pub tool: ToolKind,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub state: SessionState,
    pub pid: Option<u32>,
    /// Last activity (Unix seconds), for the panel's order only: Keo never hears it, and a
    /// change in it alone sends no event (`registry::diff`). Absent in an older state.json.
    #[serde(default)]
    pub active_at: Option<i64>,
}

/// Now, in Unix seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A session's state, as Keo's `Register` event names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Working,
    Finished,
    WaitingPermission,
    Started,
    /// Also the fallback for an unknown word in an older state.json.
    #[serde(other)]
    Idle,
}

impl SessionState {
    pub fn wire(self) -> &'static str {
        match self {
            SessionState::Working => "working",
            SessionState::Idle => "idle",
            SessionState::Finished => "finished",
            SessionState::WaitingPermission => "waiting_permission",
            SessionState::Started => "started",
        }
    }
}

/// What `start` needs. The task's words are not in it: they go in with the first `send`, after
/// the inbox's Register has tied the session to its task (Keo must hear of the session from that
/// Register first, or the session's answer stays at the machine).
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub cwd: PathBuf,
    pub name: Option<String>,
    /// The Slack thread the task came from, when Keo sent one.
    pub slack_thread: Option<String>,
}

/// An answer to a `Question` a session asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Option(String),
    Text(String),
    /// The person dismissed the question.
    Cancelled,
}

/// Everything a runner reports. The relay turns each into a Keo event; it never looks inside a
/// runner.
#[derive(Debug, Clone, PartialEq)]
pub enum RunnerEvent {
    /// A session started (by the companion or by a person).
    SessionStarted {
        tool: ToolKind,
        session_id: String,
        cwd: String,
        name: String,
    },
    /// Progress text from a running turn.
    Progress {
        tool: ToolKind,
        session_id: String,
        text: String,
    },
    /// The session waits for a person: an approval or a choice. `prompt_id` must be a fresh
    /// UUID (v4) for every question: Keo keeps an id taken for as long as it
    /// keeps the question, answered or not. Re-emit the same id only to retry the same question.
    Question {
        tool: ToolKind,
        session_id: String,
        prompt_id: String,
        text: String,
        options: Vec<String>,
    },
    /// A turn ended; `text` is the last assistant message when there is one.
    TurnEnded {
        tool: ToolKind,
        session_id: String,
        text: Option<String>,
    },
    /// A usage window reading from the tool.
    Usage {
        tool: ToolKind,
        window: String,
        percent: f64,
        resets_at: Option<String>,
    },
    /// The session is gone.
    SessionEnded {
        tool: ToolKind,
        session_id: String,
        reason: String,
    },
    /// The set of sessions changed; the registry reads `list()` again.
    RosterChanged { tool: ToolKind },
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("{0} cannot do this")]
    Unsupported(&'static str),
    #[error("{0} is not installed")]
    NotInstalled(&'static str),
    #[error("no session {0}")]
    NoSession(String),
    #[error("{0}")]
    Failed(String),
}

/// A coding tool the companion can drive. Object safe: the core holds `Arc<dyn Runner>`.
#[async_trait]
pub trait Runner: Send + Sync {
    fn kind(&self) -> ToolKind;
    /// Find the binary and read its version.
    async fn detect(&self) -> Detection;
    /// The sessions this tool has on this machine now.
    async fn list(&self) -> Result<Vec<RosterEntry>, RunnerError>;
    /// Open a new, idle session and return its id. Nothing runs, and `list()` leaves the session
    /// out, until its first `send`: the inbox's Register with the task must be the first Keo hears
    /// of it (a roster sync naming it earlier would keep its answer at the machine).
    async fn start(&self, req: StartRequest) -> Result<String, RunnerError>;
    /// Send a new user message to an idle session (for a session `start` opened, the first one).
    async fn send(&self, session_id: &str, text: &str) -> Result<(), RunnerError>;
    /// Add text to a turn that is running now (Claude Code has no mid-turn steer).
    async fn steer(&self, session_id: &str, text: &str) -> Result<(), RunnerError>;
    /// Stop the running turn.
    async fn interrupt(&self, session_id: &str) -> Result<(), RunnerError>;
    /// End a session `start` opened whose first `send` never happened (its Register failed),
    /// so no idle session is left behind. A runner whose idle sessions cost nothing keeps the
    /// default.
    async fn close(&self, _session_id: &str) -> Result<(), RunnerError> {
        Ok(())
    }
    /// Answer a `Question`.
    async fn answer(&self, prompt_id: &str, answer: Answer) -> Result<(), RunnerError>;
    /// A new receiver for this runner's events.
    fn events(&self) -> broadcast::Receiver<RunnerEvent>;
}

/// The tools a session Keo starts may use, in one place for every runner: shell, file edits, web
/// fetch and search, and Keo's own MCP server, nothing else. The person's other MCP servers
/// (computer use, browser or screen drivers among them) are not loaded, so a one-line edit never
/// turns into GUI clicks and a card for each. On a machine with an enterprise MCP policy, that
/// policy decides the MCP servers instead. Sessions the person opens themself keep their own
/// config untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPolicy {
    /// Keo's MCP server for this app's Keo host: the one MCP server a Keo-started session may
    /// load. A runner loads the person's own entry for this URL, under its own name, so the
    /// sign-in they gave carries over; with no such entry the session gets no Keo server.
    pub keo_mcp_url: String,
}

impl SessionPolicy {
    pub fn for_keo(base: &BaseUrl) -> Self {
        Self {
            keo_mcp_url: base.mcp(),
        }
    }

    /// Whether an MCP server URL is this policy's Keo server (a trailing `/` aside).
    pub fn is_keo_url(&self, url: &str) -> bool {
        url.trim_end_matches('/') == self.keo_mcp_url.trim_end_matches('/')
    }
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self::for_keo(&BaseUrl::prod())
    }
}

/// Every runner, built in one place.
pub fn all(policy: SessionPolicy) -> Vec<Arc<dyn Runner>> {
    vec![
        Arc::new(codex::CodexRunner::with_policy(policy.clone())),
        Arc::new(claude::ClaudeRunner::with_options(claude::Options {
            policy,
            ..claude::Options::default()
        })),
    ]
}

// ---- detection helpers shared by the runners ----

/// GUI apps start with a thin PATH, so the search adds the usual install folders: on macOS
/// Homebrew and the per-user tool folders, on Windows the npm global folder under `%APPDATA%`
/// (`appdata`) and the same per-user folders under the profile.
pub fn search_dirs(
    path_var: Option<&str>,
    home: Option<&Path>,
    appdata: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    if cfg!(unix) {
        for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            dirs.push(PathBuf::from(d));
        }
    }
    if let Some(appdata) = appdata {
        dirs.push(appdata.join("npm"));
    }
    if let Some(home) = home {
        for d in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
        ] {
            dirs.push(home.join(d));
        }
        dirs.extend(nvm_bins(home));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// The `bin` folders of the Node versions nvm installed, newest first: npm puts `codex` and
/// `claude` there, and a GUI app's PATH never has them.
fn nvm_bins(home: &Path) -> Vec<PathBuf> {
    let root = home.join(".nvm/versions/node");
    let mut found: Vec<(semver::Version, PathBuf)> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let v = semver::Version::parse(name.trim_start_matches('v')).ok()?;
            Some((v, e.path().join("bin")))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, d)| d).collect()
}

/// The file names a tool can have in one folder: the bare name on Unix; on Windows the native
/// `.exe`, then the `.cmd` and `.bat` shims npm installs.
pub fn binary_candidates(dir: &Path, name: &str, windows: bool) -> Vec<PathBuf> {
    if windows {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| dir.join(format!("{name}.{ext}")))
            .collect()
    } else {
        vec![dir.join(name)]
    }
}

/// This process's search folders: its PATH, then the usual install folders.
fn process_search_dirs() -> Vec<PathBuf> {
    let path_var = std::env::var("PATH").ok();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let appdata = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        None
    };
    search_dirs(path_var.as_deref(), home.as_deref(), appdata.as_deref())
}

/// The PATH for a tool's child process: the tool's own folder first (an npm launcher's
/// `#!/usr/bin/env node` needs the `node` beside it, which a GUI app's PATH lacks), then the
/// search folders.
pub fn child_path(bin: &Path) -> Option<std::ffi::OsString> {
    child_path_from(bin, process_search_dirs())
}

fn child_path_from(bin: &Path, dirs: Vec<PathBuf>) -> Option<std::ffi::OsString> {
    let mut all: Vec<PathBuf> = bin.parent().map(Path::to_path_buf).into_iter().collect();
    all.extend(dirs);
    let mut seen = std::collections::HashSet::new();
    all.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(all).ok()
}

/// The first executable file named `name` in the search folders.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    process_search_dirs()
        .into_iter()
        .flat_map(|d| binary_candidates(&d, name, cfg!(windows)))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// The first `x.y.z` in a `--version` line ("2.1.310 (Claude Code)", "codex-cli 0.160.1").
pub fn parse_version(output: &str) -> Option<semver::Version> {
    output
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .map(|w| w.trim_start_matches('v'))
        .find_map(|w| {
            let core: String = w
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if core.split('.').count() == 3 {
                semver::Version::parse(&core).ok()
            } else {
                None
            }
        })
}

/// Classify a found binary by its version against the tool's floor and tested ceiling.
pub fn classify(tool: ToolKind, path: PathBuf, version: Option<semver::Version>) -> ToolStatus {
    match version {
        None => ToolStatus::Unknown { path },
        Some(v) if v < tool.min_version() => ToolStatus::TooOld {
            path,
            version: v.to_string(),
        },
        Some(v) if tool.max_tested().is_some_and(|max| v > max) => ToolStatus::TooNew {
            path,
            version: v.to_string(),
        },
        Some(v) => ToolStatus::Ready {
            path,
            version: v.to_string(),
        },
    }
}

/// Windows' `CREATE_NO_WINDOW`: a console program the app starts opens no window of its own
/// (the release app has no console for it to share).
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The length Keo measures a question by: UTF-16 code units, as the bridge's JavaScript
/// `slice` counts them. A character outside the BMP (an emoji) is two.
pub fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that fits `max` UTF-16 units, never splitting a character.
pub fn prefix_units(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += c.len_utf16();
        if used > max {
            return &s[..i];
        }
    }
    s
}

/// Keep a tool the app starts in the background: no console window on Windows.
pub fn no_window(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Find the tool and run `<bin> --version` (5 s timeout).
pub async fn detect_tool(tool: ToolKind) -> Detection {
    let Some(path) = find_binary(tool.binary()) else {
        return Detection {
            tool,
            status: ToolStatus::NotInstalled,
        };
    };
    let mut cmd = tokio::process::Command::new(&path);
    no_window(&mut cmd);
    if let Some(p) = child_path(&path) {
        cmd.env("PATH", p);
    }
    let run = cmd
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let version = match tokio::time::timeout(Duration::from_secs(5), run).await {
        Ok(Ok(out)) => parse_version(&String::from_utf8_lossy(&out.stdout))
            .or_else(|| parse_version(&String::from_utf8_lossy(&out.stderr))),
        Ok(Err(e)) => {
            tracing::warn!(tool = tool.wire_name(), error = %e, "version check failed");
            None
        }
        Err(_) => {
            tracing::warn!(tool = tool.wire_name(), "version check timed out");
            None
        }
    };
    Detection {
        tool,
        status: classify(tool, path, version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_length_counts_bridge_units_whole_chars() {
        assert_eq!(units("ab"), 2);
        assert_eq!(units("😀"), 2);
        assert_eq!(prefix_units("a😀b", 2), "a");
        assert_eq!(prefix_units("a😀b", 3), "a😀");
        assert_eq!(prefix_units("ab", 9), "ab");
    }

    #[test]
    fn a_session_loads_only_the_keo_server_the_app_talks_to() {
        let prod = SessionPolicy::for_keo(&BaseUrl::prod());
        assert_eq!(prod.keo_mcp_url, "https://askkeo.ai/api/mcp");
        assert_eq!(SessionPolicy::default(), prod);
        assert!(prod.is_keo_url("https://askkeo.ai/api/mcp/"));
        let sub =
            SessionPolicy::for_keo(&BaseUrl::parse_allowed("https://beta.askkeo.ai").unwrap());
        assert_eq!(sub.keo_mcp_url, "https://beta.askkeo.ai/api/mcp");
        assert!(!sub.is_keo_url("https://askkeo.ai/api/mcp"));
    }

    #[test]
    fn parses_versions() {
        assert_eq!(
            parse_version("2.1.310 (Claude Code)"),
            Some(semver::Version::new(2, 1, 310))
        );
        assert_eq!(
            parse_version("codex-cli 0.160.1\n"),
            Some(semver::Version::new(0, 160, 1))
        );
        assert_eq!(parse_version("v1.2.3"), Some(semver::Version::new(1, 2, 3)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn classifies_against_floor() {
        let p = PathBuf::from("/x/claude");
        let tool = ToolKind::ClaudeCode;
        assert!(matches!(
            classify(tool, p.clone(), Some(semver::Version::new(2, 1, 302))),
            ToolStatus::TooOld { .. }
        ));
        assert!(matches!(
            classify(tool, p.clone(), Some(semver::Version::new(2, 1, 303))),
            ToolStatus::Ready { .. }
        ));
        assert!(matches!(
            classify(
                ToolKind::Codex,
                p.clone(),
                Some(semver::Version::new(0, 157, 9))
            ),
            ToolStatus::TooOld { .. }
        ));
        assert!(matches!(
            classify(
                ToolKind::Codex,
                p.clone(),
                Some(semver::Version::new(0, 166, 0))
            ),
            ToolStatus::Ready { .. }
        ));
        assert!(matches!(
            classify(
                ToolKind::Codex,
                p.clone(),
                Some(semver::Version::new(0, 166, 1))
            ),
            ToolStatus::TooNew { .. }
        ));
        assert!(matches!(
            classify(tool, p.clone(), Some(semver::Version::new(9, 0, 0))),
            ToolStatus::Ready { .. }
        ));
        assert!(matches!(
            classify(tool, p, None),
            ToolStatus::Unknown { .. }
        ));
    }

    #[test]
    fn search_dirs_add_common_folders_once() {
        let dirs = search_dirs(Some("/usr/bin:/a"), Some(Path::new("/h")), None);
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(dirs.contains(&PathBuf::from("/h/.claude/local")));
        assert_eq!(
            dirs.iter().filter(|d| *d == Path::new("/usr/bin")).count(),
            1
        );
    }

    #[test]
    fn search_dirs_end_with_nvm_node_folders_newest_first() {
        let home = tempfile::tempdir().unwrap();
        for v in ["v20.18.0", "v22.23.2", "v22.15.0", "not-a-version"] {
            std::fs::create_dir_all(home.path().join(".nvm/versions/node").join(v).join("bin"))
                .unwrap();
        }
        let dirs = search_dirs(Some("/p"), Some(home.path()), None);
        assert_eq!(dirs[0], PathBuf::from("/p"));
        let node = home.path().join(".nvm/versions/node");
        let tail: Vec<PathBuf> = dirs[dirs.len() - 3..].to_vec();
        assert_eq!(
            tail,
            vec![
                node.join("v22.23.2/bin"),
                node.join("v22.15.0/bin"),
                node.join("v20.18.0/bin"),
            ]
        );
    }

    #[test]
    fn child_path_puts_the_tools_own_folder_first() {
        let p = child_path_from(
            Path::new("/n/v22/bin/codex"),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/n/v22/bin")],
        )
        .unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&p).collect();
        assert_eq!(
            dirs,
            vec![PathBuf::from("/n/v22/bin"), PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn search_dirs_add_the_npm_folder_under_appdata() {
        let dirs = search_dirs(None, Some(Path::new("/h")), Some(Path::new("/roaming")));
        assert!(dirs.contains(&PathBuf::from("/roaming/npm")));
        assert!(dirs.contains(&PathBuf::from("/h/.local/bin")));
    }

    #[test]
    fn windows_candidates_try_exe_then_npm_shims() {
        let dir = Path::new("/d");
        assert_eq!(
            binary_candidates(dir, "codex", true),
            vec![
                PathBuf::from("/d/codex.exe"),
                PathBuf::from("/d/codex.cmd"),
                PathBuf::from("/d/codex.bat"),
            ]
        );
        assert_eq!(
            binary_candidates(dir, "claude", false),
            vec![PathBuf::from("/d/claude")]
        );
    }

    #[test]
    fn session_states_match_the_contract() {
        for (s, w) in [
            (SessionState::Working, "working"),
            (SessionState::Idle, "idle"),
            (SessionState::Finished, "finished"),
            (SessionState::WaitingPermission, "waiting_permission"),
            (SessionState::Started, "started"),
        ] {
            assert_eq!(s.wire(), w);
            assert_eq!(serde_json::to_value(s).unwrap(), w);
        }
        let old: SessionState = serde_json::from_value(serde_json::json!("busy")).unwrap();
        assert_eq!(old, SessionState::Idle);
    }

    #[test]
    fn wire_names_round_trip() {
        for t in [ToolKind::ClaudeCode, ToolKind::Codex] {
            assert_eq!(ToolKind::from_wire(t.wire_name()), Some(t));
        }
    }
}
