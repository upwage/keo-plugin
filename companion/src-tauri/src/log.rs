//! The log: one file a day under `<app data dir>/log/`, kept for 7 days.
//! Rule for every log line in the app: never task text, prompts, tokens or file contents.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

const PREFIX: &str = "companion";
const SUFFIX: &str = "log";
const KEEP_DAYS: u64 = 7;

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

/// Delete log files older than 7 days.
pub fn prune(dir: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = Duration::from_secs(KEEP_DAYS * 24 * 3600);
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_log_file(&name) {
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
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let mut chunk: Vec<String> = text.lines().map(str::to_string).collect();
        chunk.append(&mut out);
        out = chunk;
        if out.len() >= lines {
            break;
        }
    }
    let skip = out.len().saturating_sub(lines);
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
