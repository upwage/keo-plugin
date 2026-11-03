//! The log: one file a day under `<app data dir>/log/`, kept for 7 days. The Codex app-server's
//! own output goes to `codex-app-server.log` beside it, capped in size (`open_capped`) and
//! pruned by the same age.
//! Rule for every log line in the app: never task text, prompts, tokens or file contents.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const PREFIX: &str = "companion";
const SUFFIX: &str = "log";
const KEEP_DAYS: u64 = 7;
/// The Codex app-server's output, with its one older copy at `<name>.1`.
pub const CODEX_SERVER_LOG: &str = "codex-app-server.log";
/// The size past which the Codex app-server log moves to its older copy at the next start.
pub const CODEX_SERVER_LOG_MAX: u64 = 10 * 1024 * 1024;
/// The most a tail reads from one file, so one huge line cannot fill memory.
const TAIL_READ_MAX: u64 = 8 * 1024 * 1024;

pub fn log_dir(app_data: &Path) -> PathBuf {
    app_data.join("log")
}

/// Start logging to the daily file (and stderr in a dev build). Keep the guard alive.
pub fn init(dir: &Path) -> Option<WorkerGuard> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("log folder not created: {e}");
    }
    prune(dir, SystemTime::now());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(PREFIX)
        .filename_suffix(SUFFIX)
        .max_log_files(KEEP_DAYS as usize)
        .build(dir);
    let (file_layer, guard) = match appender {
        Ok(a) => {
            let (writer, guard) = tracing_appender::non_blocking(a);
            (
                Some(fmt::layer().with_ansi(false).with_writer(writer)),
                Some(guard),
            )
        }
        Err(e) => {
            eprintln!("log file not opened: {e}");
            (None, None)
        }
    };
    let stderr_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();
    guard
}

fn is_log_file(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// The files `prune` may remove: the daily logs and the Codex app-server log and its copy.
fn is_pruned(name: &str) -> bool {
    is_log_file(name) || name.starts_with(CODEX_SERVER_LOG)
}

/// `path` opened for appending. A file already past `max` bytes first becomes its one older
/// copy (`<path>.1`, replacing the last one), so the two together stay near `2 × max`.
pub fn open_capped(path: &Path, max: u64) -> std::io::Result<std::fs::File> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > max) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::remove_file(&old);
        if let Err(e) = std::fs::rename(path, &old) {
            tracing::warn!(error = %e, "log: old server log not moved aside");
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Delete log files older than 7 days.
pub fn prune(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = Duration::from_secs(KEEP_DAYS * 24 * 3600);
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_pruned(&name) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > cutoff);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The last `lines` lines across the newest log files.
pub fn tail(dir: &Path, lines: usize) -> Vec<String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(is_log_file)
                })
                .collect()
        })
        .unwrap_or_default();
    // Daily names sort by date.
    files.sort();
    let mut out: Vec<String> = Vec::new();
    for file in files.iter().rev() {
        if out.len() >= lines {
            break;
        }
        let mut chunk = last_lines(file, lines - out.len());
        chunk.append(&mut out);
        out = chunk;
    }
    out
}

/// The last `n` lines of `path`, read back from its end a block at a time: only the blocks those
/// lines sit in are read (at most `TAIL_READ_MAX` bytes), never the whole file.
fn last_lines(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: u64 = 64 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut start = len;
    let mut buf: Vec<u8> = Vec::new();
    let mut breaks = 0;
    // `n` whole lines need `n` line breaks before them, beyond the file's own last one.
    while start > 0 && breaks <= n && len - start < TAIL_READ_MAX {
        let step = BLOCK.min(start);
        start -= step;
        let mut block = vec![0u8; step as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut block).is_err() {
            break;
        }
        breaks += block.iter().filter(|&&b| b == b'\n').count();
        block.extend_from_slice(&buf);
        buf = block;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut out: Vec<String> = text.lines().map(str::to_string).collect();
    // A read that stopped short of the file's start begins inside a line.
    if start > 0 && !out.is_empty() {
        out.remove(0);
    }
    let skip = out.len().saturating_sub(n);
    out.split_off(skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_reads_across_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("companion.2026-01-01.log"), "a\nb\n").unwrap();
        std::fs::write(dir.path().join("companion.2026-01-02.log"), "c\nd\n").unwrap();
        std::fs::write(dir.path().join("other.file"), "zzz\n").unwrap();
        assert_eq!(tail(dir.path(), 3), vec!["b", "c", "d"]);
        assert_eq!(tail(dir.path(), 10), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn tail_reads_only_the_end_of_a_big_file() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("companion.2026-01-01.log"), &body).unwrap();
        assert_eq!(
            tail(dir.path(), 3),
            vec!["line 49997", "line 49998", "line 49999"]
        );
        // A line that spans a block edge comes back whole.
        let n = 100_000 / "line 00000\n".len() + 7;
        let want: Vec<String> = (50_000 - n..50_000).map(|i| format!("line {i}")).collect();
        assert_eq!(tail(dir.path(), n), want);
        assert_eq!(
            last_lines(&dir.path().join("none.log"), 3),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_codex_server_log_is_capped_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CODEX_SERVER_LOG);
        std::fs::write(&path, "x".repeat(20)).unwrap();
        // Under the cap it keeps growing.
        drop(open_capped(&path, 100).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 20);
        // Past it, the next start begins a new file and keeps one older copy.
        drop(open_capped(&path, 10).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let old = dir.path().join(format!("{CODEX_SERVER_LOG}.1"));
        assert_eq!(std::fs::metadata(&old).unwrap().len(), 20);
        // The tail never shows it; prune removes both once old.
        assert!(tail(dir.path(), 5).is_empty());
        let later = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        assert_eq!(prune(dir.path(), later), 2);
    }

    #[test]
    fn prune_keeps_recent_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("companion.2026-01-01.log"), "x").unwrap();
        // Nothing is old yet.
        assert_eq!(prune(dir.path(), SystemTime::now()), 0);
        // Eight days later it is.
        let later = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        assert_eq!(prune(dir.path(), later), 1);
    }
}
