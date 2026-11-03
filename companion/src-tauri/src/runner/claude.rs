//! The Claude Code runner.
//!
//! - Roster: the sessions this runner started, with their folder and name, each once its first
//!   message went in (`start` opens an idle session; the inbox registers it with its task, then
//!   sends the words). Sessions a person opened themself are not listed: Keo would then route
//!   their Slack messages to the companion, which cannot type into them. Their hooks (the Keo
//!   plugin and its doorbell, unchanged) stay their channel.
//! - Start: the companion owns the session as a child process,
//!   `claude -p --input-format stream-json --output-format stream-json --verbose
//!   --include-hook-events --permission-prompt-tool stdio --permission-mode auto --session-id <v4>
//!   --name <name>`, in the approved folder. The Slack Allow that started it is the person's one
//!   approval: auto mode runs each tool call without asking (Claude Code's own safety check
//!   stops the risky ones and tells the model why), so web fetches, searches, edits and commands
//!   do not become Slack cards. `claude --bg` cannot be used: it refuses `-p`, and it has no stream, so
//!   no progress and no questions (both recorded on Claude Code 2.1.316). Without
//!   `--permission-prompt-tool stdio` every permission ask is denied at once.
//! - Questions: the stream's `control_request` `can_use_tool` (a permission ask that still
//!   reaches the host: its card says which mode the session is in) and
//!   `elicitation` (an MCP server's form), each with a fresh v4 `prompt_id`. `answer()` writes the
//!   `control_response`; no answer in `QUESTION_TIMEOUT` declines.
//! - Turn end: the `result` frame (read `is_error` and `terminal_reason`, never `subtype`).
//!   Usage: the `rate_limit_event` frame's `unifiedWindows`.
//! - Interrupt: a `control_request` `interrupt`; the turn ends with an error `result`.
//! - Steer: Claude Code has no mid-turn steer. The text waits for the turn's end and goes in as
//!   the next message, and `steer` answers `Unsupported`, so the card can offer "interrupt and
//!   re-prompt" instead. An interrupt drops the waiting text: the re-prompt carries it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::broadcast;

use super::{
    child_path, detect_tool, find_binary, now_secs, Answer, Detection, RosterEntry, Runner,
    RunnerError, RunnerEvent, SessionPolicy, SessionState, StartRequest, ToolKind,
};

const KIND: ToolKind = ToolKind::ClaudeCode;

/// Keo cancels an unanswered question after 30 minutes; this is the runner's own backstop.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(35 * 60);
/// A write to a session's input that takes longer gives the session up.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
/// The permission mode a Keo-started session runs in: one Allow in Slack, then no per-tool asks.
const PERMISSION_MODE: &str = "auto";
/// Longest description or message quoted in a question.
const QUOTE_MAX: usize = 600;
/// Keo's cap on a question's text (the Codex runner's `MAX_TEXT`).
const QUESTION_MAX: usize = 2_000;
/// Said when a permission ask cannot be shown whole in one Keo question: Allow must never
/// approve what the person could not read, so only Deny stays.
const TOO_LONG: &str = "\nThis request cannot be shown here in full, so Slack can only deny it.";

/// Variables a session started from inside Claude Code would inherit and misread as "nested".
const NESTED_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Knobs the tests turn; `Default` is the product.
#[derive(Clone)]
pub struct Options {
    /// The `claude` binary; `None` searches the usual folders.
    pub binary: Option<PathBuf>,
    pub question_timeout: Duration,
    pub policy: SessionPolicy,
    /// The person's Claude Code config (`.claude.json`); `None` reads the usual place.
    pub claude_config: Option<PathBuf>,
    /// The machine's enterprise MCP policy file; `None` checks the usual place.
    pub managed_mcp: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            binary: None,
            question_timeout: QUESTION_TIMEOUT,
            policy: SessionPolicy::default(),
            claude_config: None,
            managed_mcp: None,
        }
    }
}

pub struct ClaudeRunner {
    inner: Arc<Inner>,
}

struct Inner {
    opts: Options,
    events: broadcast::Sender<RunnerEvent>,
    children: Mutex<HashMap<String, Child>>,
    prompts: Mutex<HashMap<String, Pending>>,
    /// Last reading per usage window, so an unchanged one is not sent again.
    usage: Mutex<HashMap<String, (i64, Option<String>)>>,
}

/// A session this runner started and owns.
struct Child {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    cwd: String,
    name: String,
    pid: Option<u32>,
    /// Shown in `list()` once its first message went in.
    announced: bool,
    busy: bool,
    /// Last turn start or end (Unix seconds), for the panel's order.
    active_at: i64,
    /// Steer text waiting for the turn's end.
    queued: Vec<String>,
}

/// An open question: what the session asked and how to answer it.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    session_id: String,
    request_id: String,
    ask: Ask,
}

#[derive(Debug, Clone, PartialEq)]
enum Ask {
    /// A permission ask; `input` goes back unchanged on Allow. `allow` is false when the
    /// question could not show the request in full: then every answer denies.
    Tool { input: Value, allow: bool },
    /// An MCP elicitation form, read by `form::parse` once when it arrives.
    Elicit { form: Form, options: Vec<String> },
}

/// What an elicitation answer may send back.
#[derive(Debug, Clone, PartialEq)]
enum Form {
    /// A form this cannot read in full (no schema, another mode, a rule nobody here checks, or
    /// several fields): every answer declines.
    DeclineOnly,
    /// A form of no fields: Accept sends empty content.
    Confirm,
    /// A form of one field: an answer must be a value it takes.
    One(super::form::Field),
}

impl ClaudeRunner {
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    pub fn with_options(opts: Options) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                opts,
                events,
                children: Mutex::new(HashMap::new()),
                prompts: Mutex::new(HashMap::new()),
                usage: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl Default for ClaudeRunner {
    fn default() -> Self {
        Self::new()
    }
}

// ---- pure helpers: the stream, the roster and the answers ----

/// A fresh random (v4) UUID.
fn uuid_v4() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// `secs` since 1970 as an ISO time in UTC (civil-from-days, so no date crate is needed).
fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `s` cut to `max` UTF-16 units (how Keo measures a question), marking the cut.
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if super::units(s) <= max {
        return s.to_string();
    }
    let mut out = super::prefix_units(s, max.saturating_sub(1)).to_string();
    out.push('…');
    out
}

/// The session name: the one asked for, else the Slack thread, else the folder.
fn session_name(req: &StartRequest) -> String {
    if let Some(n) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    if let Some(t) = req.slack_thread.as_deref().filter(|t| !t.is_empty()) {
        return format!("keo {t}");
    }
    format!(
        "keo {}",
        crate::registry::session_name(&req.cwd.display().to_string())
    )
}

/// The built-in tools a Keo-started session gets (`SessionPolicy`): shell, files, web fetch and
/// search, subagents, and `ToolSearch` so Keo's MCP tools load when asked for.
const LEAN_TOOLS: &str = if cfg!(windows) {
    // Native Windows runs shell commands through its own PowerShell tool.
    "Bash,PowerShell,Read,Edit,Write,NotebookEdit,WebFetch,WebSearch,Task,TaskStop,ToolSearch,Skill"
} else {
    "Bash,Read,Edit,Write,NotebookEdit,WebFetch,WebSearch,Task,TaskStop,ToolSearch,Skill"
};

/// Where Claude Code keeps the person's config: `$CLAUDE_CONFIG_DIR/.claude.json`, else
/// `.claude.json` in the home folder (`HOME`, else `USERPROFILE` on Windows).
fn claude_config_path() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("HOME"))
        .or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(dir).join(".claude.json"))
}

/// Where an administrator deploys Claude Code's enterprise MCP policy (`managed-mcp.json`).
fn managed_mcp_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-mcp.json")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode\managed-mcp.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-mcp.json")
    }
}

/// Whether the enterprise MCP policy at `path` is in effect: Claude Code applies it only when it
/// can read the file and parse it as a JSON object, so anything else keeps strict mode.
fn managed_mcp_in_effect(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|v| v.is_object())
}

/// The person's Claude config. A missing file is an empty config; a file that cannot be read
/// or parsed is an error, so the log says so instead of "no Keo server".
fn read_claude_config(path: &std::path::Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("not valid JSON: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(e) => Err(e.to_string()),
    }
}

/// The person's own MCP server entry for Keo's URL, with its name, as Claude Code would load it
/// in `cwd`: a local entry (keyed by the git root, or by the folder's real path outside a repo,
/// as Claude Code keys a project) overrides a user entry of the same name, and only then is
/// Keo's URL looked for. Passed on unchanged, so the sign-in stored for
/// it carries over. A server that only the Keo plugin declares is not found here: its sign-in
/// is stored under the plugin's id, which a re-declared server would not reuse.
fn own_keo_server(
    policy: &SessionPolicy,
    config: &Value,
    cwd: &std::path::Path,
) -> Option<(String, Value)> {
    // `dunce` drops the `\\?\` prefix Windows' canonical form adds, which no project key has.
    let real = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let projects = config.get("projects");
    // Claude Code keys a project by its git root, and by the folder itself outside a repo, so a
    // subfolder's own entry inside a repo is not the one it loads.
    let keys: Vec<&std::path::Path> = match real.ancestors().find(|d| d.join(".git").exists()) {
        Some(root) => vec![root],
        None => vec![real.as_path(), cwd],
    };
    let local = keys.into_iter().find_map(|p| {
        let key = p.to_str()?;
        // Tried as written and with forward slashes, the two forms a Windows key may take.
        [key.to_string(), key.replace('\\', "/")]
            .iter()
            .find_map(|k| projects?.get(k)?.get("mcpServers")?.as_object())
    });
    let user = config.get("mcpServers").and_then(Value::as_object);
    let mut effective: Vec<(&String, &Value)> = local.into_iter().flatten().collect();
    if let Some(user) = user {
        effective.extend(
            user.iter()
                .filter(|(n, _)| !local.is_some_and(|l| l.contains_key(*n))),
        );
    }
    effective
        .into_iter()
        .find(|(_, s)| {
            s.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| policy.is_keo_url(u))
        })
        .map(|(name, s)| (name.clone(), s.clone()))
}

/// A private file holding the one MCP server a session may load, so a credential in its entry
/// (a header) never shows in the process list. Removed when dropped (the session ended, or it
/// never started).
struct McpConfigFile(PathBuf);

impl McpConfigFile {
    fn write(session_id: &str, name: &str, server: &Value) -> std::io::Result<Self> {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("keo-companion-mcp-{session_id}.json"));
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let mut file = open.open(&path)?;
        let file_guard = Self(path);
        file.write_all(json!({"mcpServers": {name: server}}).to_string().as_bytes())?;
        Ok(file_guard)
    }
}

impl Drop for McpConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Which MCP servers a session loads.
enum Mcp<'a> {
    /// `--strict-mcp-config`: none of the person's (user, project, plugin, claude.ai) servers,
    /// only the file given, which holds their own Keo entry.
    Strict(Option<&'a std::path::Path>),
    /// The machine has an enterprise MCP policy, which decides the servers: Claude Code exits
    /// at start if strict mode or `--mcp-config` is passed with one deployed.
    Managed,
}

/// Flags that hold a session to its tool policy: the MCP servers `mcp` names; no Chrome
/// driver; the lean built-in tool list. Hooks still load, so the Keo plugin's doorbell keeps
/// working.
fn policy_args(mcp: Mcp) -> Vec<String> {
    let mut args = Vec::new();
    if let Mcp::Strict(keo) = mcp {
        args.push("--strict-mcp-config".to_string());
        if let Some(file) = keo {
            args.push("--mcp-config".into());
            args.push(file.display().to_string());
        }
    }
    args.extend(["--no-chrome".into(), "--tools".into(), LEAN_TOOLS.into()]);
    args
}

/// The user frame that sends `text` as the next message.
fn user_frame(text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": text},
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

/// What one stream frame means for the runner.
#[derive(Debug, PartialEq)]
enum Out {
    Event(RunnerEvent),
    /// A question to ask; the runner adds the prompt id.
    Ask {
        request_id: String,
        ask: Ask,
        text: String,
        options: Vec<String>,
    },
    /// The session dropped a question (its turn moved on).
    Cancelled {
        request_id: String,
    },
    /// A control request the runner does not handle: answer it with an error.
    Refuse {
        request_id: String,
    },
    /// The turn ended.
    TurnDone,
}

/// One session's stream, read frame by frame.
struct Stream {
    session_id: String,
    last_text: Option<String>,
    last_progress: Option<String>,
    /// The permission mode the session reported at start (`system` `init`).
    mode: Option<String>,
}

impl Stream {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            last_text: None,
            last_progress: None,
            mode: None,
        }
    }

    fn on_frame(&mut self, f: &Value) -> Vec<Out> {
        let s = |k: &str| f.get(k).and_then(Value::as_str);
        match s("type") {
            Some("assistant") => {
                let text: Vec<&str> = f
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .filter(|t| !t.trim().is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    self.last_text = Some(text.join("\n\n"));
                }
                Vec::new()
            }
            Some("system") => match s("subtype") {
                Some("init") => {
                    self.mode = s("permissionMode").map(str::to_string);
                    Vec::new()
                }
                // A short line Claude Code writes for each step: exactly a progress note.
                Some("task_summary") => {
                    let Some(detail) = s("detail").map(str::trim).filter(|d| !d.is_empty()) else {
                        return Vec::new();
                    };
                    if self.last_progress.as_deref() == Some(detail) {
                        return Vec::new();
                    }
                    self.last_progress = Some(detail.to_string());
                    vec![Out::Event(RunnerEvent::Progress {
                        tool: KIND,
                        session_id: self.session_id.clone(),
                        text: detail.to_string(),
                    })]
                }
                _ => Vec::new(),
            },
            Some("rate_limit_event") => usage_of(f),
            Some("control_request") => {
                let request_id = s("request_id").unwrap_or_default().to_string();
                let r = f.get("request").cloned().unwrap_or(Value::Null);
                match r.get("subtype").and_then(Value::as_str) {
                    Some("can_use_tool") => {
                        let (text, options) = tool_question(&r, self.mode.as_deref());
                        let allow = options.iter().any(|o| o == ALLOW);
                        vec![Out::Ask {
                            request_id,
                            ask: Ask::Tool {
                                input: r.get("input").cloned().unwrap_or_else(|| json!({})),
                                allow,
                            },
                            text,
                            options,
                        }]
                    }
                    Some("elicitation") => {
                        let (text, form, options) = elicit_question(&r);
                        vec![Out::Ask {
                            request_id,
                            ask: Ask::Elicit {
                                form,
                                options: options.clone(),
                            },
                            text,
                            options,
                        }]
                    }
                    _ => vec![Out::Refuse { request_id }],
                }
            }
            Some("control_cancel_request") => vec![Out::Cancelled {
                request_id: s("request_id").unwrap_or_default().to_string(),
            }],
            Some("result") => {
                let is_error = f.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let result = s("result")
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string);
                let text = match (result, self.last_text.take()) {
                    (Some(r), _) => Some(r),
                    (None, Some(t)) if !is_error => Some(t),
                    (None, last) => {
                        if s("subtype") == Some("error_during_execution") {
                            Some("Stopped before the turn finished.".to_string())
                        } else if is_error {
                            Some(format!(
                                "The turn failed ({}).",
                                s("terminal_reason").unwrap_or("error")
                            ))
                        } else {
                            last
                        }
                    }
                };
                self.last_progress = None;
                vec![
                    Out::Event(RunnerEvent::TurnEnded {
                        tool: KIND,
                        session_id: self.session_id.clone(),
                        text,
                    }),
                    Out::TurnDone,
                ]
            }
            _ => Vec::new(),
        }
    }
}

/// One `Usage` per window in a `rate_limit_event` (`utilization` is 0–1, `resetsAt` seconds).
fn usage_of(f: &Value) -> Vec<Out> {
    let Some(windows) = f
        .pointer("/rate_limit_info/unifiedWindows")
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out: Vec<Out> = windows
        .iter()
        .filter_map(|(name, w)| {
            let utilization = w.get("utilization")?.as_f64()?;
            Some(Out::Event(RunnerEvent::Usage {
                tool: KIND,
                window: name.clone(),
                percent: (utilization * 100.0).clamp(0.0, 100.0),
                resets_at: w.get("resetsAt").and_then(Value::as_i64).map(iso_utc),
            }))
        })
        .collect();
    out.sort_by_key(|o| match o {
        Out::Event(RunnerEvent::Usage { window, .. }) => window.clone(),
        _ => String::new(),
    });
    out
}

const ALLOW: &str = "Allow";
const DENY: &str = "Deny";
const ACCEPT: &str = "Accept";
const DECLINE: &str = "Decline";

/// Why a session started in `PERMISSION_MODE` still asked about a tool, for the card: the mode it
/// actually runs in (`mode`, from its `init` frame) and what that means.
fn mode_note(mode: Option<&str>) -> String {
    match mode {
        Some(PERMISSION_MODE) => {
            "Auto mode is on, and Claude Code still asked about this step.".to_string()
        }
        Some(m) => {
            format!("This session runs in {m} mode, not auto mode, so it asks before a tool.")
        }
        None => "Claude Code did not say which permission mode it runs in, so it asks before a \
             tool."
            .to_string(),
    }
}

/// The permission question. Allow sends the whole `input` back as approved, so the text shows
/// all of it as it is: the main field (command, path, URL or pattern) quoted, every other field
/// as `key: value` JSON. When that does not fit one Keo question, or the main field cannot be
/// quoted without changing it, the text is cut and only Deny is offered.
fn tool_question(r: &Value, mode: Option<&str>) -> (String, Vec<String>) {
    let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::trim);
    // The tool Allow approves is `tool_name`: it is always named, and a display name only adds to
    // it. With no tool name, or an input that is not an object, nothing granted is known.
    let tool_name = s("tool_name").filter(|t| !t.is_empty());
    let mut shown = tool_name.is_some() && r.get("input").is_some_and(Value::is_object);
    let tool = match (s("display_name").filter(|t| !t.is_empty()), tool_name) {
        (Some(d), Some(t)) if d != t => format!("{d} ({t})"),
        (_, Some(t)) => t.to_string(),
        (Some(d), None) => d.to_string(),
        (None, None) => "a tool".to_string(),
    };
    let mut text = format!("Claude Code wants to use {tool}");
    let description = s("description").filter(|d| !d.is_empty());
    if let Some(d) = description {
        text.push_str(&format!(": {d}"));
    }
    let empty = Map::new();
    let input = r.get("input").unwrap_or(&Value::Null);
    let fields = input.as_object().unwrap_or(&empty);
    let main = ["command", "file_path", "path", "url", "pattern"]
        .iter()
        .find(|k| {
            fields
                .get(**k)
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
        });
    if !input.is_object() && !input.is_null() {
        text.push_str(&format!("\n{input}"));
    }
    if let Some(key) = main {
        let v = fields[*key].as_str().unwrap_or("");
        if !v.contains(['`', '\n']) && v.trim() == v {
            text.push_str(&format!("\n`{v}`"));
        } else if !v.contains("```") {
            text.push_str(&format!("\n```\n{v}\n```"));
        } else {
            text.push_str(&format!("\n{}", clip(&v.replace('`', "'"), QUOTE_MAX)));
            shown = false;
        }
    }
    for (k, v) in fields {
        // The main field is quoted above; Bash's own description is the one already shown.
        let repeated = k == "description" && v.as_str().map(str::trim) == description;
        if main.is_some_and(|m| m == k) || repeated {
            continue;
        }
        text.push_str(&format!("\n{k}: {v}"));
    }
    let note = format!("\n{}", mode_note(mode));
    let whole = format!("{text}{note}");
    if shown && super::units(&whole) <= QUESTION_MAX {
        return (whole, vec![ALLOW.to_string(), DENY.to_string()]);
    }
    let room = QUESTION_MAX.saturating_sub(super::units(&note) + super::units(TOO_LONG) + 1);
    (
        format!("{}{note}{TOO_LONG}", clip(&text, room.max(1))),
        vec![DENY.to_string()],
    )
}

/// The question for an elicitation, what its answer may send, and its options. The schema is
/// checked by `form::parse`, the same check the Codex runner makes: anything outside the subset
/// it reads can only be declined.
fn elicit_question(r: &Value) -> (String, Form, Vec<String>) {
    let message = r
        .get("message")
        .and_then(Value::as_str)
        .map(|m| clip(m, QUOTE_MAX))
        .unwrap_or_else(|| "A tool asks for an answer.".to_string());
    let mut text = match r.get("mcp_server_name").and_then(Value::as_str) {
        Some(server) if !server.is_empty() => format!("{message} (asked by {server})"),
        _ => message,
    };
    // A form is the protocol's default mode; any other (a link to open, say) is not read here.
    let form_mode = r.get("mode").map_or(true, |m| m.as_str() == Some("form"));
    let fields = r
        .get("requested_schema")
        .filter(|_| form_mode)
        .map(super::form::parse);
    let form = match fields {
        Some(Ok(fields)) => match <[_; 1]>::try_from(fields) {
            Ok([field]) => Form::One(field),
            Err(fields) if fields.is_empty() => Form::Confirm,
            Err(_) => Form::DeclineOnly,
        },
        _ => Form::DeclineOnly,
    };
    let options = match &form {
        Form::DeclineOnly => {
            text.push_str("\nThis form asks for an answer Slack cannot give: answer it at the machine, or decline it here.");
            vec![DECLINE.to_string()]
        }
        Form::Confirm => vec![ACCEPT.to_string(), DECLINE.to_string()],
        Form::One(f) if f.kind == super::form::Kind::Boolean => {
            vec!["Yes".to_string(), "No".to_string()]
        }
        Form::One(f) => f.values.iter().map(|v| super::form::label(v)).collect(),
    };
    (text, form, options)
}

/// The words of an answer, with an option's index read as its label.
fn answer_words(answer: &Answer, options: &[String]) -> Option<String> {
    match answer {
        Answer::Cancelled => None,
        Answer::Text(t) => Some(t.trim().to_string()),
        Answer::Option(o) => {
            let o = o.trim();
            // A label first (an option may itself be "1"), then a label Keo clipped, then an index.
            if options.iter().any(|x| x == o) {
                return Some(o.to_string());
            }
            let stem = o.trim_end_matches('…');
            let clipped: Vec<&String> = options
                .iter()
                .filter(|x| stem.len() < x.len() && x.starts_with(stem) && !stem.is_empty())
                .collect();
            if let [only] = clipped.as_slice() {
                return Some((*only).clone());
            }
            Some(
                o.parse::<usize>()
                    .ok()
                    .and_then(|i| options.get(i).cloned())
                    .unwrap_or_else(|| o.to_string()),
            )
        }
    }
}

/// The `response` body of the `control_response` that answers `ask`.
fn response_for(ask: &Ask, answer: &Answer) -> Value {
    match ask {
        Ask::Tool { input, allow } => {
            let words = answer_words(answer, &[ALLOW.to_string(), DENY.to_string()]);
            let allow = *allow
                && words.as_deref().is_some_and(|w| {
                    ["allow", "yes", "approve", "ok"].contains(&w.to_lowercase().as_str())
                });
            if allow {
                return json!({"behavior": "allow", "updatedInput": input});
            }
            let message = match (answer, words) {
                (Answer::Cancelled, _) => {
                    "No answer came from Slack in time, so this was denied.".to_string()
                }
                (Answer::Text(_), Some(w)) if !w.is_empty() && !w.eq_ignore_ascii_case(DENY) => {
                    format!("Denied from Slack: {w}")
                }
                _ => "Denied from Slack.".to_string(),
            };
            json!({"behavior": "deny", "message": message})
        }
        Ask::Elicit { form, options } => {
            let decline = json!({"action": "decline"});
            let Some(words) = answer_words(answer, options) else {
                return decline;
            };
            if words.eq_ignore_ascii_case(DECLINE) {
                return decline;
            }
            match form {
                Form::DeclineOnly => decline,
                Form::Confirm if words.eq_ignore_ascii_case(ACCEPT) => {
                    json!({"action": "accept", "content": {}})
                }
                Form::Confirm => decline,
                // Only a value the field takes is sent: a listed value (a shortened label maps
                // back to the one value it stands for), a whole or finite number, yes or no.
                Form::One(field) => match super::form::coerce(field, &words) {
                    Some(value) => {
                        json!({"action": "accept", "content": {field.name.clone(): value}})
                    }
                    None => decline,
                },
            }
        }
    }
}

// ---- the runner ----

impl Inner {
    fn binary(&self) -> Result<PathBuf, RunnerError> {
        self.opts
            .binary
            .clone()
            .or_else(|| find_binary(KIND.binary()))
            .ok_or(RunnerError::NotInstalled(KIND.display_name()))
    }

    fn emit(&self, ev: RunnerEvent) {
        let _ = self.events.send(ev);
    }

    /// A `claude` command with the GUI's thin PATH widened and nested-session variables removed.
    fn command(&self, bin: &PathBuf) -> Command {
        let mut cmd = Command::new(bin);
        super::no_window(&mut cmd);
        if let Some(path) = child_path(bin) {
            cmd.env("PATH", path);
        }
        for v in NESTED_ENV {
            cmd.env_remove(v);
        }
        cmd.kill_on_drop(true);
        cmd
    }

    async fn write(&self, session_id: &str, frame: &Value) -> Result<(), RunnerError> {
        let stdin = self
            .children
            .lock()
            .expect("children lock")
            .get(session_id)
            .map(|c| Arc::clone(&c.stdin))
            .ok_or_else(|| RunnerError::NoSession(session_id.to_string()))?;
        let mut line = frame.to_string();
        line.push('\n');
        let wrote = tokio::time::timeout(WRITE_TIMEOUT, async {
            let mut w = stdin.lock().await;
            w.write_all(line.as_bytes()).await?;
            w.flush().await
        })
        .await;
        match wrote {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(RunnerError::Failed(format!("session input closed: {e}"))),
            Err(_) => {
                // A session that stops reading its input is given up: dropping its input closes
                // it, so the process ends and its waiter reports the session ended.
                self.children
                    .lock()
                    .expect("children lock")
                    .remove(session_id);
                Err(RunnerError::Failed(
                    "session stopped reading its input".into(),
                ))
            }
        }
    }

    /// Send `text` as the session's next message. The first one also puts the session on the
    /// roster (`start` keeps it off until then).
    async fn send_user(&self, session_id: &str, text: &str) -> Result<(), RunnerError> {
        self.write(session_id, &user_frame(text)).await?;
        let listed = match self
            .children
            .lock()
            .expect("children lock")
            .get_mut(session_id)
        {
            Some(c) => {
                c.busy = true;
                c.active_at = now_secs();
                c.announced = true;
                true
            }
            None => false,
        };
        // The first message lists the session; every later one turns it to working.
        if listed {
            self.emit(RunnerEvent::RosterChanged { tool: KIND });
        }
        Ok(())
    }

    /// The roster's state for session `id` changed (a turn ended, a question opened or closed):
    /// tell the registry, once the session is listed.
    fn roster_changed(&self, id: &str) {
        let listed = self
            .children
            .lock()
            .expect("children lock")
            .get(id)
            .is_some_and(|c| c.announced);
        if listed {
            self.emit(RunnerEvent::RosterChanged { tool: KIND });
        }
    }

    /// The sessions this runner started, once their first message went in.
    fn roster(&self) -> Vec<RosterEntry> {
        let children = self.children.lock().expect("children lock");
        let prompts = self.prompts.lock().expect("prompts lock");
        let mut list: Vec<RosterEntry> = children
            .iter()
            .filter(|(_, c)| c.announced)
            .map(|(id, c)| RosterEntry {
                tool: KIND,
                session_id: id.clone(),
                cwd: c.cwd.clone(),
                name: c.name.clone(),
                state: own_state(c, id, &prompts),
                pid: c.pid,
                active_at: Some(c.active_at),
            })
            .collect();
        list.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        list
    }

    /// Handle what one frame of session `id` means.
    async fn on_out(self: &Arc<Self>, id: &str, out: Out) {
        match out {
            Out::Event(RunnerEvent::Usage {
                tool,
                window,
                percent,
                resets_at,
            }) => {
                let key = (percent.round() as i64, resets_at.clone());
                let fresh = self
                    .usage
                    .lock()
                    .expect("usage lock")
                    .insert(window.clone(), key.clone())
                    != Some(key);
                if fresh {
                    self.emit(RunnerEvent::Usage {
                        tool,
                        window,
                        percent,
                        resets_at,
                    });
                }
            }
            Out::Event(ev) => self.emit(ev),
            Out::Ask {
                request_id,
                ask,
                text,
                options,
            } => {
                let prompt_id = uuid_v4();
                self.prompts.lock().expect("prompts lock").insert(
                    prompt_id.clone(),
                    Pending {
                        session_id: id.to_string(),
                        request_id,
                        ask,
                    },
                );
                self.emit(RunnerEvent::Question {
                    tool: KIND,
                    session_id: id.to_string(),
                    prompt_id: prompt_id.clone(),
                    text,
                    options,
                });
                self.roster_changed(id);
                let me = Arc::downgrade(self);
                let wait = self.opts.question_timeout;
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    if let Some(me) = me.upgrade() {
                        if me.answer(&prompt_id, Answer::Cancelled).await.is_ok() {
                            tracing::info!("claude: question timed out; declined");
                        }
                    }
                });
            }
            Out::Cancelled { request_id } => {
                self.prompts
                    .lock()
                    .expect("prompts lock")
                    .retain(|_, p| !(p.session_id == id && p.request_id == request_id));
                self.roster_changed(id);
            }
            Out::Refuse { request_id } => {
                let frame = json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": "The Keo companion does not handle this request.",
                    },
                });
                if let Err(e) = self.write(id, &frame).await {
                    tracing::warn!(error = %e, "claude: could not refuse a control request");
                }
            }
            Out::TurnDone => {
                let queued = {
                    let mut children = self.children.lock().expect("children lock");
                    let Some(c) = children.get_mut(id) else {
                        return;
                    };
                    c.busy = false;
                    c.active_at = now_secs();
                    std::mem::take(&mut c.queued)
                };
                self.roster_changed(id);
                if !queued.is_empty() {
                    if let Err(e) = self.send_user(id, &queued.join("\n\n")).await {
                        tracing::warn!(error = %e, "claude: queued message not sent");
                    }
                }
            }
        }
    }

    async fn answer(&self, prompt_id: &str, answer: Answer) -> Result<(), RunnerError> {
        let pending = self
            .prompts
            .lock()
            .expect("prompts lock")
            .remove(prompt_id)
            .ok_or_else(|| RunnerError::NoSession(prompt_id.to_string()))?;
        let frame = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": pending.request_id,
                "response": response_for(&pending.ask, &answer),
            },
        });
        let sent = self.write(&pending.session_id, &frame).await;
        if sent.is_err() {
            // Not delivered to a live session: the question stays open, so a retry can still
            // answer it. A session already gone keeps nothing. Checked under the children lock
            // (taken before the prompts lock, as `roster` does), so an exit's cleanup always
            // runs after this.
            let children = self.children.lock().expect("children lock");
            if children.contains_key(&pending.session_id) {
                self.prompts
                    .lock()
                    .expect("prompts lock")
                    .insert(prompt_id.to_string(), pending);
            }
        } else {
            self.roster_changed(&pending.session_id);
        }
        sent
    }
}

fn own_state(c: &Child, id: &str, prompts: &HashMap<String, Pending>) -> SessionState {
    if prompts.values().any(|p| p.session_id == id) {
        SessionState::WaitingPermission
    } else if c.busy {
        SessionState::Working
    } else {
        SessionState::Idle
    }
}

#[async_trait]
impl Runner for ClaudeRunner {
    fn kind(&self) -> ToolKind {
        KIND
    }

    async fn detect(&self) -> Detection {
        match &self.inner.opts.binary {
            None => detect_tool(KIND).await,
            Some(path) => Detection {
                tool: KIND,
                status: super::ToolStatus::Unknown { path: path.clone() },
            },
        }
    }

    async fn list(&self) -> Result<Vec<RosterEntry>, RunnerError> {
        Ok(self.inner.roster())
    }

    async fn start(&self, req: StartRequest) -> Result<String, RunnerError> {
        let inner = &self.inner;
        let bin = inner.binary()?;
        let id = uuid_v4();
        let name = session_name(&req);
        let config = match inner.opts.claude_config.clone().or_else(claude_config_path) {
            Some(path) => read_claude_config(&path).unwrap_or_else(|e| {
                tracing::warn!(session = %id, error = %e, "claude: the person's Claude config could not be read; no Keo MCP server for the session");
                Value::Null
            }),
            None => Value::Null,
        };
        let managed = managed_mcp_in_effect(
            &inner
                .opts
                .managed_mcp
                .clone()
                .unwrap_or_else(managed_mcp_path),
        );
        let mcp_file = match own_keo_server(&inner.opts.policy, &config, &req.cwd) {
            _ if managed => {
                tracing::info!(session = %id, "claude: session gets the machine's managed MCP servers (an enterprise MCP policy is deployed)");
                None
            }
            Some((server, entry)) => match McpConfigFile::write(&id, &server, &entry) {
                Ok(file) => {
                    tracing::info!(session = %id, server = %server, "claude: session gets the person's Keo MCP server");
                    Some(file)
                }
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "claude: Keo MCP server file not written; no Keo MCP server for the session");
                    None
                }
            },
            None => {
                tracing::info!(
                    session = %id,
                    "claude: session gets no Keo MCP server (none in the person's Claude config; a plugin-only sign-in is not reused)"
                );
                None
            }
        };
        let mut cmd = inner.command(&bin);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-hook-events",
            "--permission-prompt-tool",
            "stdio",
            "--permission-mode",
            PERMISSION_MODE,
            "--session-id",
            &id,
            "--name",
            &name,
        ])
        .args(policy_args(if managed {
            Mcp::Managed
        } else {
            Mcp::Strict(mcp_file.as_ref().map(|f| f.0.as_path()))
        }))
        .current_dir(&req.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| RunnerError::Failed(format!("could not start Claude Code: {e}")))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let cwd = req.cwd.display().to_string();
        inner.children.lock().expect("children lock").insert(
            id.clone(),
            Child {
                stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
                cwd,
                name,
                pid: child.id(),
                announced: false,
                busy: false,
                active_at: now_secs(),
                queued: Vec::new(),
            },
        );
        let init = json!({"type": "control_request", "request_id": format!("init-{id}"),
                          "request": {"subtype": "initialize"}});
        if let Err(e) = inner.write(&id, &init).await {
            // Dropping the child kills it (`kill_on_drop`).
            inner.children.lock().expect("children lock").remove(&id);
            return Err(e);
        }

        // The stream: one frame per line.
        let me = Arc::clone(inner);
        let sid = id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut stream = Stream::new(&sid);
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                for out in stream.on_frame(&frame) {
                    me.on_out(&sid, out).await;
                }
            }
        });
        // stderr: logged, and its last line kept as the exit reason.
        let tail = Arc::new(Mutex::new(String::new()));
        let tail_w = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %clip(&line, 300), "claude stderr");
                if !line.trim().is_empty() {
                    *tail_w.lock().expect("tail lock") = clip(&line, 300);
                }
            }
        });
        // The process's end is the session's end.
        let me = Arc::clone(inner);
        let sid = id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            drop(mcp_file);
            // Let the stream reader hand over the last frames first.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let announced = me
                .children
                .lock()
                .expect("children lock")
                .remove(&sid)
                .is_some_and(|c| c.announced);
            me.prompts
                .lock()
                .expect("prompts lock")
                .retain(|_, p| p.session_id != sid);
            let tail = tail.lock().expect("tail lock").clone();
            let reason = match status {
                Ok(s) if s.success() => "exited".to_string(),
                Ok(s) if tail.is_empty() => format!("exited ({s})"),
                Ok(s) => format!("exited ({s}): {tail}"),
                Err(e) => format!("lost: {e}"),
            };
            me.emit(RunnerEvent::SessionEnded {
                tool: KIND,
                session_id: sid,
                reason,
            });
            if announced {
                me.emit(RunnerEvent::RosterChanged { tool: KIND });
            }
        });
        Ok(id)
    }

    async fn send(&self, session_id: &str, text: &str) -> Result<(), RunnerError> {
        self.inner.send_user(session_id, text).await
    }

    async fn steer(&self, session_id: &str, text: &str) -> Result<(), RunnerError> {
        let busy = {
            let mut children = self.inner.children.lock().expect("children lock");
            let c = children
                .get_mut(session_id)
                .ok_or_else(|| RunnerError::NoSession(session_id.to_string()))?;
            if c.busy {
                c.queued.push(text.to_string());
            }
            c.busy
        };
        if !busy {
            // No turn runs: the text is simply the next message.
            return self.inner.send_user(session_id, text).await;
        }
        Err(RunnerError::Unsupported(KIND.display_name()))
    }

    async fn interrupt(&self, session_id: &str) -> Result<(), RunnerError> {
        self.inner
            .children
            .lock()
            .expect("children lock")
            .get_mut(session_id)
            .map(|c| c.queued.clear())
            .ok_or_else(|| RunnerError::NoSession(session_id.to_string()))?;
        let frame = json!({"type": "control_request", "request_id": uuid_v4(),
                           "request": {"subtype": "interrupt"}});
        self.inner.write(session_id, &frame).await
    }

    async fn answer(&self, prompt_id: &str, answer: Answer) -> Result<(), RunnerError> {
        self.inner.answer(prompt_id, answer).await
    }

    async fn close(&self, session_id: &str) -> Result<(), RunnerError> {
        // Dropping its input closes it: the process ends, and its waiter reports the end.
        self.inner
            .children
            .lock()
            .expect("children lock")
            .remove(session_id)
            .map(drop)
            .ok_or_else(|| RunnerError::NoSession(session_id.to_string()))
    }

    fn events(&self) -> broadcast::Receiver<RunnerEvent> {
        self.inner.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(src: &str) -> Vec<Value> {
        src.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fixture line"))
            .collect()
    }

    fn run(src: &str) -> Vec<Out> {
        let mut s = Stream::new("s1");
        frames(src).iter().flat_map(|f| s.on_frame(f)).collect()
    }

    /// A permission ask that reaches the host in auto mode says auto mode is on; one with no
    /// `init` before it says the mode is unknown.
    #[test]
    fn a_permission_ask_names_the_mode_it_came_from() {
        let ask = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}});
        let text_of = |outs: Vec<Out>| match outs.into_iter().next() {
            Some(Out::Ask { text, .. }) => text,
            other => panic!("not an ask: {other:?}"),
        };
        let mut s = Stream::new("s1");
        assert!(s
            .on_frame(&json!({"type": "system", "subtype": "init", "permissionMode": "auto"}))
            .is_empty());
        let text = text_of(s.on_frame(&ask));
        assert_eq!(
            text,
            "Claude Code wants to use Bash\n`ls`\nAuto mode is on, and Claude Code still asked about this step."
        );
        let text = text_of(Stream::new("s2").on_frame(&ask));
        assert!(text
            .ends_with("did not say which permission mode it runs in, so it asks before a tool."));
    }

    /// Allow approves the whole input, so a command shows whole; one too long for a question
    /// can only be denied from Slack.
    #[test]
    fn a_command_too_long_to_show_whole_can_only_be_denied() {
        let ask_for = |command: &str| {
            let f = json!({"type": "control_request", "request_id": "r1", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": command}}});
            match Stream::new("s1").on_frame(&f).into_iter().next() {
                Some(Out::Ask {
                    ask, text, options, ..
                }) => (ask, text, options),
                other => panic!("not an ask: {other:?}"),
            }
        };
        let long = format!("echo {}; touch hidden-step", "a".repeat(900));
        let (ask, text, options) = ask_for(&long);
        assert!(text.contains("touch hidden-step"), "shown whole: {text}");
        assert_eq!(options, vec![ALLOW, DENY]);
        assert_eq!(
            response_for(&ask, &Answer::Option(ALLOW.into()))["behavior"],
            "allow"
        );
        let huge = format!("echo {}; touch hidden-step", "a".repeat(3_000));
        let (ask, text, options) = ask_for(&huge);
        assert!(text.chars().count() <= QUESTION_MAX);
        assert!(!text.contains("hidden-step"));
        assert!(text.ends_with("so Slack can only deny it."));
        assert_eq!(options, vec![DENY]);
        // Keo cuts a question at 2000 UTF-16 units: an emoji counts two, so ~1000 of them under
        // 2000 characters still hide the tail, and the question is deny-only.
        let emoji = format!("echo {}; touch hidden-step", "😀".repeat(990));
        assert!(emoji.chars().count() < QUESTION_MAX);
        let (_, text, options) = ask_for(&emoji);
        assert!(super::super::units(&text) <= QUESTION_MAX);
        assert!(!text.contains("hidden-step"));
        assert_eq!(options, vec![DENY]);
        // Every field Allow would send back is in the text, so none hides behind the command.
        let f = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "description": "List",
            "input": {"command": "ls", "description": "List", "dangerouslyDisableSandbox": true}}});
        let Some(Out::Ask { text, options, .. }) =
            Stream::new("s1").on_frame(&f).into_iter().next()
        else {
            panic!("not an ask")
        };
        assert!(text.contains("\ndangerouslyDisableSandbox: true"), "{text}");
        assert!(
            !text.contains("description:"),
            "the shown description is not repeated"
        );
        assert_eq!(options, vec![ALLOW, DENY]);
        // A long description is shown whole too, or the question is deny-only.
        let f = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "description": "d".repeat(2_500),
            "input": {"command": "ls"}}});
        let Some(Out::Ask { options, .. }) = Stream::new("s1").on_frame(&f).into_iter().next()
        else {
            panic!("not an ask")
        };
        assert_eq!(options, vec![DENY]);
        // A command that breaks its quoting cannot be shown as it is.
        let (_, _, options) = ask_for("echo ``` touch hidden-step");
        assert_eq!(options, vec![DENY]);
        let (_, text, options) = ask_for("printf 'a`b'");
        assert!(text.contains("```\nprintf 'a`b'\n```"), "{text}");
        assert_eq!(options, vec![ALLOW, DENY]);
        // The tool Allow approves is always named; with no tool name or no input object,
        // nothing granted is known.
        for request in [
            json!({"subtype": "can_use_tool", "display_name": "Read", "input": {"file_path": "a"}}),
            json!({"subtype": "can_use_tool", "tool_name": "Bash"}),
            json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": "ls"}),
        ] {
            let f = json!({"type": "control_request", "request_id": "r1", "request": request});
            let Some(Out::Ask { options, ask, .. }) =
                Stream::new("s1").on_frame(&f).into_iter().next()
            else {
                panic!("not an ask")
            };
            assert_eq!(options, vec![DENY], "{f}");
            assert_eq!(
                response_for(&ask, &Answer::Option(ALLOW.into()))["behavior"],
                "deny"
            );
        }
        let f = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "display_name": "Read",
            "input": {"command": "ls"}}});
        let Some(Out::Ask { text, .. }) = Stream::new("s1").on_frame(&f).into_iter().next() else {
            panic!("not an ask")
        };
        assert!(
            text.starts_with("Claude Code wants to use Read (Bash)"),
            "{text}"
        );
        for a in [ALLOW, "yes", "0"] {
            assert_eq!(
                response_for(&ask, &Answer::Option(a.into()))["behavior"],
                "deny",
                "{a}"
            );
        }
    }

    fn turn_text(outs: &[Out]) -> Vec<Option<String>> {
        outs.iter()
            .filter_map(|o| match o {
                Out::Event(RunnerEvent::TurnEnded { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn permission_ask_becomes_an_allow_deny_question() {
        let outs = run(include_str!("fixtures/claude/permission-allow.ndjson.json"));
        let asks: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Out::Ask {
                    request_id,
                    ask,
                    text,
                    options,
                } => Some((request_id, ask, text, options)),
                _ => None,
            })
            .collect();
        assert_eq!(asks.len(), 1);
        let (request_id, ask, text, options) = asks[0];
        assert_eq!(request_id, "98bfd709-047e-4450-abab-daeda6b48c60");
        assert!(text.starts_with("Claude Code wants to use Bash: Write spike-permission"));
        assert!(text.contains("`echo spike-permission > spike-perm.txt`"));
        // Recorded in default mode: the card says auto mode is not what this session runs in.
        assert!(text.ends_with(
            "This session runs in default mode, not auto mode, so it asks before a tool."
        ));
        assert_eq!(options, &vec!["Allow".to_string(), "Deny".to_string()]);
        // Allow sends the input back unchanged, as the spike's host did.
        let allow = response_for(ask, &Answer::Option("Allow".into()));
        assert_eq!(allow["behavior"], "allow");
        assert_eq!(
            allow["updatedInput"]["command"],
            "echo spike-permission > spike-perm.txt"
        );
        assert_eq!(
            response_for(ask, &Answer::Option("0".into()))["behavior"],
            "allow"
        );
        // Progress from the step summary, then the turn's answer.
        assert!(outs.iter().any(|o| matches!(o,
            Out::Event(RunnerEvent::Progress { text, .. })
                if text == "Writing spike-permission text to spike-perm.txt")));
        assert_eq!(
            turn_text(&outs),
            vec![Some(
                "Ran `echo spike-permission > spike-perm.txt` in the working directory; it finished with no output."
                    .to_string()
            )]
        );
        assert!(matches!(outs.last(), Some(Out::TurnDone)));
    }

    #[test]
    fn deny_and_timeout_decline_the_tool() {
        let outs = run(include_str!("fixtures/claude/permission-deny.ndjson.json"));
        let ask = outs
            .iter()
            .find_map(|o| match o {
                Out::Ask { ask, .. } => Some(ask.clone()),
                _ => None,
            })
            .expect("a question");
        assert_eq!(
            response_for(&ask, &Answer::Option("Deny".into())),
            json!({"behavior": "deny", "message": "Denied from Slack."})
        );
        assert_eq!(
            response_for(&ask, &Answer::Option("1".into()))["behavior"],
            "deny"
        );
        let timeout = response_for(&ask, &Answer::Cancelled);
        assert_eq!(timeout["behavior"], "deny");
        assert!(timeout["message"].as_str().unwrap().contains("in time"));
        let why = response_for(&ask, &Answer::Text("use the other folder".into()));
        assert_eq!(why["message"], "Denied from Slack: use the other folder");
        assert_eq!(turn_text(&outs).len(), 1);
    }

    #[test]
    fn elicitation_offers_the_enum_and_answers_the_form() {
        let outs = run(include_str!("fixtures/claude/elicitation.ndjson.json"));
        let (ask, text, options) = outs
            .iter()
            .find_map(|o| match o {
                Out::Ask {
                    ask, text, options, ..
                } => Some((ask.clone(), text.clone(), options.clone())),
                _ => None,
            })
            .expect("a question");
        assert_eq!(text, "Which color do you want? (asked by keo-spike-elicit)");
        assert_eq!(options, vec!["red".to_string(), "blue".to_string()]);
        // The spike's host answered exactly this.
        assert_eq!(
            response_for(&ask, &Answer::Option("blue".into())),
            json!({"action": "accept", "content": {"color": "blue"}})
        );
        assert_eq!(
            response_for(&ask, &Answer::Option("0".into())),
            json!({"action": "accept", "content": {"color": "red"}})
        );
        assert_eq!(
            response_for(&ask, &Answer::Text("green".into())),
            json!({"action": "decline"})
        );
        assert_eq!(
            response_for(&ask, &Answer::Cancelled),
            json!({"action": "decline"})
        );
    }

    #[test]
    fn elicitation_forms_of_other_shapes() {
        let ask = |schema: Value| {
            let r = json!({"message": "m", "mode": "form", "requested_schema": schema});
            let (text, form, options) = elicit_question(&r);
            (text, Ask::Elicit { form, options })
        };
        let (_, free) = ask(json!({"type": "object", "properties": {"why": {"type": "string"}}}));
        assert_eq!(
            response_for(&free, &Answer::Text("because".into())),
            json!({"action": "accept", "content": {"why": "because"}})
        );
        let (_, flag) = ask(json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}));
        assert_eq!(
            response_for(&flag, &Answer::Option("1".into())),
            json!({"action": "accept", "content": {"ok": false}})
        );
        let (_, n) = ask(json!({"type": "object", "properties": {"n": {"type": "number"}}}));
        assert_eq!(
            response_for(&n, &Answer::Text("NaN".into())),
            json!({"action": "decline"})
        );
        let (_, confirm) = ask(json!({"type": "object", "properties": {}}));
        assert_eq!(
            response_for(&confirm, &Answer::Option("Accept".into())),
            json!({"action": "accept", "content": {}})
        );
        assert_eq!(
            response_for(&confirm, &Answer::Text("sure".into())),
            json!({"action": "decline"})
        );
        // No schema, another mode, several fields, a root that is no object form, or a rule
        // nobody here checks: Decline is the one option and every answer declines.
        let r = json!({"message": "m", "mode": "url", "requested_schema": {"type": "object", "properties": {}}});
        let (_, form, options) = elicit_question(&r);
        let mut cases = vec![(Ask::Elicit { form, options }, "url mode")];
        let (_, form, options) = elicit_question(&json!({"message": "m"}));
        cases.push((Ask::Elicit { form, options }, "no schema"));
        for (schema, why) in [
            (
                json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}}),
                "several",
            ),
            (
                json!({"properties": {"ok": {"type": "boolean"}}}),
                "no root type",
            ),
            (json!({"type": "string", "properties": {}}), "string root"),
            (
                json!({"type": "object", "properties": {"ok": {"type": "boolean", "enum": [true]}}}),
                "boolean enum",
            ),
            (
                json!({"type": "object", "properties": {"s": {"type": "string", "pattern": "^a"}}}),
                "pattern",
            ),
            (
                json!({"type": "object", "properties": {"c": {"type": "string", "enum": []}}}),
                "empty enum",
            ),
        ] {
            cases.push((ask(schema).1, why));
        }
        for (a, why) in cases {
            let Ask::Elicit { options, .. } = &a else {
                unreachable!()
            };
            assert_eq!(options, &vec![DECLINE.to_string()], "{why}");
            for words in ["Accept", "yes", "No", r#"{"a":"1","b":"2"}"#, "0"] {
                assert_eq!(
                    response_for(&a, &Answer::Text(words.into())),
                    json!({"action": "decline"}),
                    "{why}: {words}"
                );
            }
        }
    }

    #[test]
    fn a_failed_turn_reads_is_error_not_subtype() {
        // `subtype: success` with `is_error: true`: the synthetic auth error is the answer.
        let outs = run(include_str!("fixtures/claude/stream.ndjson.json"));
        assert_eq!(
            turn_text(&outs),
            vec![Some(
                "Failed to authenticate: OAuth session expired and could not be refreshed"
                    .to_string()
            )]
        );
        let mut s = Stream::new("s1");
        s.last_text = Some("half an answer".into());
        let outs = s.on_frame(&json!({"type": "result", "subtype": "success",
            "is_error": true, "terminal_reason": "api_error"}));
        assert_eq!(
            turn_text(&outs),
            vec![Some("The turn failed (api_error).".to_string())]
        );
    }

    #[test]
    fn an_interrupted_turn_ends_and_the_queued_one_follows() {
        // Recorded on 2.1.316: interrupt mid-tool, then the message sent meanwhile runs.
        let outs = run(include_str!("fixtures/claude/interrupt.ndjson.json"));
        assert_eq!(
            turn_text(&outs),
            vec![
                Some("Stopped before the turn finished.".to_string()),
                Some("second turn ok.".to_string())
            ]
        );
        assert_eq!(
            outs.iter().filter(|o| matches!(o, Out::TurnDone)).count(),
            2
        );
        assert!(!outs.iter().any(|o| matches!(o, Out::Ask { .. })));
    }

    #[test]
    fn rate_limit_event_is_a_usage_reading() {
        let outs = run(include_str!("fixtures/claude/permission-allow.ndjson.json"));
        let usage: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Out::Event(RunnerEvent::Usage {
                    window,
                    percent,
                    resets_at,
                    ..
                }) => Some((window.clone(), *percent, resets_at.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            usage,
            vec![(
                "seven_day".to_string(),
                38.0,
                Some("2026-11-07T12:00:00Z".to_string())
            )]
        );
    }

    #[test]
    fn progress_repeats_are_dropped_and_unknown_requests_refused() {
        let mut s = Stream::new("s1");
        let p = json!({"type": "system", "subtype": "task_summary", "detail": "Reading files"});
        assert_eq!(s.on_frame(&p).len(), 1);
        assert!(s.on_frame(&p).is_empty());
        assert!(s
            .on_frame(&json!({"type": "system", "subtype": "task_summary", "detail": null}))
            .is_empty());
        assert_eq!(
            s.on_frame(&json!({"type": "control_request", "request_id": "r9",
                "request": {"subtype": "mcp_message"}})),
            vec![Out::Refuse {
                request_id: "r9".into()
            }]
        );
        assert_eq!(
            s.on_frame(&json!({"type": "control_cancel_request", "request_id": "r9"})),
            vec![Out::Cancelled {
                request_id: "r9".into()
            }]
        );
    }

    #[test]
    fn a_session_reuses_only_the_persons_own_keo_server() {
        let policy = SessionPolicy {
            keo_mcp_url: "https://beta.askkeo.ai/api/mcp".into(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let real = dunce::canonicalize(tmp.path()).unwrap().join("app");
        std::fs::create_dir_all(&real).unwrap();
        let link = dunce::canonicalize(tmp.path()).unwrap().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let config = json!({
            "mcpServers": {
                "other": {"type": "http", "url": "https://example.com/mcp"},
                "my-keo": {"type": "http", "url": "https://beta.askkeo.ai/api/mcp/"},
                "shadowed": {"type": "http", "url": "https://beta.askkeo.ai/api/mcp"},
            },
            "projects": {real.to_str().unwrap(): {"mcpServers": {
                "keo-here": {"type": "http", "url": "https://beta.askkeo.ai/api/mcp", "headers": {}},
            }}},
        });
        // The local entry for the folder's real path wins, reached through a symlink too.
        let here = own_keo_server(&policy, &config, &real).unwrap();
        assert_eq!(here.0, "keo-here");
        assert_eq!(here.1["headers"], json!({}));
        #[cfg(unix)]
        assert_eq!(
            own_keo_server(&policy, &config, &link).unwrap().0,
            "keo-here"
        );
        // Elsewhere, the first user entry for Keo's URL.
        assert_eq!(
            own_keo_server(&policy, &config, std::path::Path::new("/w/else"))
                .unwrap()
                .0,
            "my-keo"
        );
        // A subfolder of a git repo finds the entry Claude Code keys by the repo's root.
        std::fs::create_dir_all(real.join(".git")).unwrap();
        std::fs::create_dir_all(real.join("packages/web")).unwrap();
        assert_eq!(
            own_keo_server(&policy, &config, &real.join("packages/web"))
                .unwrap()
                .0,
            "keo-here"
        );
        // A subfolder's own entry, even an empty one, does not mask the root's: Claude Code
        // never loads it inside a repo.
        let mut masked = config.clone();
        masked["projects"][real.join("packages/web").to_str().unwrap()] = json!({"mcpServers": {}});
        assert_eq!(
            own_keo_server(&policy, &masked, &real.join("packages/web"))
                .unwrap()
                .0,
            "keo-here"
        );
        // A local non-Keo server shadows the user Keo entry of the same name.
        let shadow = json!({
            "mcpServers": {"keo": {"type": "http", "url": "https://beta.askkeo.ai/api/mcp"}},
            "projects": {real.to_str().unwrap(): {"mcpServers": {
                "keo": {"type": "http", "url": "https://example.com/mcp"},
            }}},
        });
        assert_eq!(own_keo_server(&policy, &shadow, &real), None);
        // Plugin-only (or none): no Keo server, still strict.
        assert_eq!(
            own_keo_server(&policy, &json!({"mcpServers": {}}), &real),
            None
        );
        assert_eq!(own_keo_server(&policy, &Value::Null, &real), None);

        // A missing config is empty; a broken one is an error.
        assert_eq!(
            read_claude_config(&tmp.path().join("none.json")),
            Ok(Value::Null)
        );
        std::fs::write(tmp.path().join("bad.json"), "{").unwrap();
        assert!(read_claude_config(&tmp.path().join("bad.json")).is_err());

        // The chosen entry goes in a private file, never on the command line, and the file goes
        // when the session does.
        let file = McpConfigFile::write(&uuid_v4(), &here.0, &here.1).unwrap();
        let path = file.0.clone();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["keo-here"], here.1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Shell, files, web, subagents and skills; PowerShell too on Windows. Nothing else.
        let expected = if cfg!(windows) {
            "Bash,PowerShell,Read,Edit,Write,NotebookEdit,WebFetch,WebSearch,Task,TaskStop,ToolSearch,Skill"
        } else {
            "Bash,Read,Edit,Write,NotebookEdit,WebFetch,WebSearch,Task,TaskStop,ToolSearch,Skill"
        };
        assert_eq!(LEAN_TOOLS, expected);
        let lean = [
            "--no-chrome".to_string(),
            "--tools".into(),
            LEAN_TOOLS.into(),
        ];
        assert_eq!(
            policy_args(Mcp::Strict(None)),
            [vec!["--strict-mcp-config".to_string()], lean.to_vec()].concat()
        );
        assert_eq!(
            policy_args(Mcp::Strict(Some(&path))),
            [
                vec![
                    "--strict-mcp-config".to_string(),
                    "--mcp-config".into(),
                    path.display().to_string(),
                ],
                lean.to_vec()
            ]
            .concat()
        );
        // An enterprise MCP policy decides the servers; strict mode would make Claude Code exit.
        assert_eq!(policy_args(Mcp::Managed), lean.to_vec());
        // Only a policy file Claude Code can read and parse is in effect.
        let managed = tmp.path().join("managed-mcp.json");
        assert!(!managed_mcp_in_effect(&managed));
        std::fs::write(&managed, "not json").unwrap();
        assert!(!managed_mcp_in_effect(&managed));
        std::fs::write(&managed, r#"{"mcpServers": {}}"#).unwrap();
        assert!(managed_mcp_in_effect(&managed));
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn helpers() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(uuid_v4(), id);
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_794_052_800), "2026-11-07T12:00:00Z");
        assert_eq!(iso_utc(951_782_400), "2000-02-29T00:00:00Z");
        let opts = |o: &[&str]| o.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // A label wins over an index, a clipped label finds its option, an index its label.
        assert_eq!(
            answer_words(&Answer::Option("1".into()), &opts(&["1", "2"])),
            Some("1".into())
        );
        assert_eq!(
            answer_words(
                &Answer::Option("a long lab…".into()),
                &opts(&["a long label", "b"])
            ),
            Some("a long label".into())
        );
        assert_eq!(
            answer_words(&Answer::Option("1".into()), &opts(&["red", "blue"])),
            Some("blue".into())
        );
        assert_eq!(answer_words(&Answer::Cancelled, &opts(&["red"])), None);
        let req = |name: Option<&str>, thread: Option<&str>| StartRequest {
            cwd: PathBuf::from("/w/keo-app"),
            name: name.map(str::to_string),
            slack_thread: thread.map(str::to_string),
        };
        assert_eq!(
            session_name(&req(Some("TICKET-1"), Some("C1/2"))),
            "TICKET-1"
        );
        assert_eq!(session_name(&req(None, Some("C1/2.3"))), "keo C1/2.3");
        assert_eq!(session_name(&req(None, None)), "keo keo-app");
    }

    /// A fake `claude`: records its arguments and input, plays `turn1.ndjson` on the first message and `turn2.ndjson` on the
    /// first permission answer, then exits.
    #[cfg(unix)]
    fn fake_claude(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = r#"#!/bin/sh
D="$(dirname "$0")"
printf '%s\n' "$*" > "$D/args"
pwd > "$D/cwd"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$D/stdin"
  case "$line" in
    *'"type":"user"'*) cat "$D/turn1.ndjson";;
    *'"behavior"'*) cat "$D/turn2.ndjson"; exit 0;;
  esac
done
"#;
        let bin = dir.join("claude");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    async fn next(rx: &mut broadcast::Receiver<RunnerEvent>) -> RunnerEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an event in time")
            .expect("open channel")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_started_session_asks_is_answered_and_ends() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        let work = tempfile::tempdir().unwrap();
        // Frames recorded on 2.1.316 (permission-allow.ndjson.json), split at the permission ask.
        let all = include_str!("fixtures/claude/permission-allow.ndjson.json");
        let at = all.find(r#""type":"control_request""#).unwrap();
        let cut = all[..at].rfind('\n').unwrap() + 1;
        let end = cut + all[cut..].find('\n').unwrap() + 1;
        std::fs::write(dir.path().join("turn1.ndjson"), &all[..end]).unwrap();
        std::fs::write(dir.path().join("turn2.ndjson"), &all[end..]).unwrap();
        // The person's Claude config holds their own Keo server (and another one, left out).
        let config = dir.path().join("claude.json");
        std::fs::write(
            &config,
            r#"{"mcpServers":{"gui":{"type":"stdio","command":"x"},"keo":{"type":"http","url":"https://askkeo.ai/api/mcp"}}}"#,
        )
        .unwrap();

        let runner = ClaudeRunner::with_options(Options {
            binary: Some(bin),
            question_timeout: Duration::from_secs(60),
            claude_config: Some(config),
            managed_mcp: Some(dir.path().join("no-managed-mcp.json")),
            ..Options::default()
        });
        let mut rx = runner.events();
        let id = runner
            .start(StartRequest {
                cwd: work.path().to_path_buf(),
                name: None,
                slack_thread: Some("C123/1700000000.000100".into()),
            })
            .await
            .unwrap();
        // Idle and off the roster until its first message: the inbox registers it first.
        assert!(runner.list().await.unwrap().is_empty());
        assert!(matches!(
            runner.interrupt("nope").await,
            Err(RunnerError::NoSession(_))
        ));
        assert!(matches!(
            runner.send("nope", "hi").await,
            Err(RunnerError::NoSession(_))
        ));
        runner.send(&id, "write the file").await.unwrap();
        assert_eq!(
            next(&mut rx).await,
            RunnerEvent::RosterChanged { tool: KIND }
        );

        let mut prompt = None;
        let mut seen = Vec::new();
        while prompt.is_none() {
            match next(&mut rx).await {
                RunnerEvent::Question {
                    prompt_id,
                    session_id,
                    options,
                    ..
                } => {
                    assert_eq!(session_id, id);
                    assert_eq!(options, vec!["Allow".to_string(), "Deny".to_string()]);
                    prompt = Some(prompt_id);
                    // The waiting state reaches the saved roster and the panel.
                    assert_eq!(
                        next(&mut rx).await,
                        RunnerEvent::RosterChanged { tool: KIND }
                    );
                }
                ev => seen.push(ev),
            }
        }
        assert!(seen
            .iter()
            .any(|e| matches!(e, RunnerEvent::Usage { window, .. } if window == "seven_day")));
        let roster = runner.list().await.unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].session_id, id);
        assert_eq!(roster[0].name, "keo C123/1700000000.000100");
        assert_eq!(roster[0].state, SessionState::WaitingPermission);
        assert!(roster[0].active_at > Some(0), "the panel orders by it");
        let prompt = prompt.unwrap();
        // A steer mid-turn waits and says so.
        assert!(matches!(
            runner.steer(&id, "also add a test").await,
            Err(RunnerError::Unsupported(_))
        ));
        runner
            .answer(&prompt, Answer::Option("Allow".into()))
            .await
            .unwrap();
        assert!(matches!(
            runner.answer(&prompt, Answer::Option("Allow".into())).await,
            Err(RunnerError::NoSession(_))
        ));

        let mut ended = None;
        let mut session_end = None;
        let mut roster_changes = 0;
        while session_end.is_none() {
            match next(&mut rx).await {
                RunnerEvent::TurnEnded { text, .. } => ended = Some(text),
                RunnerEvent::SessionEnded { reason, .. } => session_end = Some(reason),
                RunnerEvent::RosterChanged { .. } => roster_changes += 1,
                _ => {}
            }
        }
        // The answer (no longer waiting) and the turn's end (no longer working) each re-list.
        assert!(roster_changes >= 2, "{roster_changes} roster changes");
        assert!(ended
            .flatten()
            .unwrap()
            .starts_with("Ran `echo spike-permission"));
        assert_eq!(session_end.unwrap(), "exited");

        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        for a in [
            "-p --input-format stream-json --output-format stream-json --verbose --include-hook-events --permission-prompt-tool stdio --permission-mode auto",
            &format!("--session-id {id}"),
            "--name keo C123/1700000000.000100",
            "--strict-mcp-config --mcp-config ",
            &format!("keo-companion-mcp-{id}.json --no-chrome --tools {LEAN_TOOLS}"),
        ] {
            assert!(args.contains(a), "{a} missing from {args}");
        }
        assert!(
            !args.contains("gui") && !args.contains("askkeo"),
            "server config on the command line: {args}"
        );
        // The session ended: its server file is gone.
        assert!(!std::env::temp_dir()
            .join(format!("keo-companion-mcp-{id}.json"))
            .exists());
        let cwd = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
        assert!(cwd
            .trim()
            .ends_with(work.path().file_name().unwrap().to_str().unwrap()));
        let input: Vec<Value> = std::fs::read_to_string(dir.path().join("stdin"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(input[0]["request"]["subtype"], "initialize");
        assert_eq!(input[1]["message"]["content"], "write the file");
        assert_eq!(
            input[2]["response"]["request_id"],
            "98bfd709-047e-4450-abab-daeda6b48c60"
        );
        assert_eq!(input[2]["response"]["response"]["behavior"], "allow");
        // The queued steer went in at the turn's end, before the fake exited or after; either
        // way the session is gone now.
        assert!(runner.list().await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unused_session_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path());
        let work = tempfile::tempdir().unwrap();
        let runner = ClaudeRunner::with_options(Options {
            binary: Some(bin),
            ..Options::default()
        });
        let mut rx = runner.events();
        let id = runner
            .start(StartRequest {
                cwd: work.path().to_path_buf(),
                name: Some("unused".into()),
                slack_thread: None,
            })
            .await
            .unwrap();
        runner.close(&id).await.unwrap();
        // It ends, never having joined the roster.
        assert!(matches!(
            next(&mut rx).await,
            RunnerEvent::SessionEnded { session_id, .. } if session_id == id
        ));
        assert!(runner.list().await.unwrap().is_empty());
        assert!(matches!(
            runner.close(&id).await,
            Err(RunnerError::NoSession(_))
        ));
    }

    #[tokio::test]
    async fn an_answer_for_a_session_already_gone_keeps_nothing() {
        let runner = ClaudeRunner::with_options(Options::default());
        runner.inner.prompts.lock().unwrap().insert(
            "p1".into(),
            Pending {
                session_id: "gone".into(),
                request_id: "r1".into(),
                ask: Ask::Tool {
                    input: json!({}),
                    allow: true,
                },
            },
        );
        assert!(matches!(
            runner.answer("p1", Answer::Option("Allow".into())).await,
            Err(RunnerError::NoSession(_))
        ));
        assert!(runner.inner.prompts.lock().unwrap().is_empty());
    }
}
