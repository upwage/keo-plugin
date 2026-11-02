//! The coding-tool versions this build drives, in one place. The runners read them through
//! `ToolKind::min_version` and `ToolKind::max_tested`; out of range, the tray shows "needs update"
//! and the panel says what to update.
//!
//! To support a new Codex release: run the runner tests and a real start, send and question on
//! it, then raise `CODEX_MAX_TESTED` in the same PR.

/// The oldest Claude Code the companion drives.
pub const CLAUDE_CODE_MIN: semver::Version = semver::Version::new(2, 1, 303);
/// The newest Claude Code tested with this build. `None`: no ceiling (its CLI flags are stable).
pub const CLAUDE_CODE_MAX_TESTED: Option<semver::Version> = None;

/// The oldest Codex the companion drives.
pub const CODEX_MIN: semver::Version = semver::Version::new(0, 158, 0);
/// The newest Codex tested with this build. The app-server protocol is experimental, so a newer
/// Codex waits until a companion release has been tested with it.
pub const CODEX_MAX_TESTED: Option<semver::Version> = Some(semver::Version::new(0, 166, 0));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_are_not_below_floors() {
        if let Some(max) = &CLAUDE_CODE_MAX_TESTED {
            assert!(*max >= CLAUDE_CODE_MIN);
        }
        if let Some(max) = &CODEX_MAX_TESTED {
            assert!(*max >= CODEX_MIN);
        }
    }
}
