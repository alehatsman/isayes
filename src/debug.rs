//! The debug log. Spec §15.
//!
//! `ISAYES_DEBUG=1` appends to `~/.isayes-debug.log`. Unset, nothing is opened
//! and every call here is a branch on an `Option`.
//!
//! This is not a nicety. §6's table is pinned to Claude Code's current dialog
//! strings, Anthropic owns those and does not version them, and a change to one
//! drops the score below the threshold and stops the tool working **with no
//! error at all**. That is the worst failure mode in this design, and the only
//! thing standing between it and an afternoon of guessing is a log that says
//! which indicator went quiet.
//!
//! So the format optimises for one question — *why did it not fire?* — and
//! logs every score above zero, not just the ones that crossed.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::detector::Detection;
use crate::ignore;

/// Set this to `1` to turn the log on.
pub const ENV_VAR: &str = "ISAYES_DEBUG";

/// An append-only log, or nothing at all.
#[derive(Debug)]
pub struct DebugLog {
    file: Option<std::fs::File>,
    started: SystemTime,
}

impl DebugLog {
    /// Open the log if [`ENV_VAR`] is `1`, otherwise return a sink that does
    /// nothing. Never fails: a debug log that takes the process down with it
    /// would be worse than no debug log.
    #[must_use]
    pub fn open() -> Self {
        let mut log = Self {
            file: None,
            started: SystemTime::now(),
        };
        if std::env::var(ENV_VAR).as_deref() != Ok("1") {
            return log;
        }
        let Some(path) = log_path() else {
            return log;
        };
        log.file = OpenOptions::new().create(true).append(true).open(path).ok();
        log.line("=== session started ===");
        log
    }

    /// Is anything actually being written? Callers use this to skip building
    /// a message that would be thrown away.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }

    /// One line, stamped with seconds since this log opened.
    ///
    /// Elapsed rather than wall clock: the question is always "what happened
    /// just before it stopped", which is a relative question, and it keeps the
    /// formatting to one `{:.3}` with no date library.
    pub fn line(&mut self, message: &str) {
        let elapsed = self
            .started
            .elapsed()
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        if let Some(file) = self.file.as_mut() {
            ignore(writeln!(file, "[{elapsed:9.3}] {message}"));
            ignore(file.flush());
        }
    }

    /// What the detector concluded, and *why*.
    ///
    /// Scores of zero are skipped — that is ordinary output and would bury
    /// everything else. Anything above zero is logged even when it did not
    /// cross, because a dialog that suddenly scores 2 instead of 5 is exactly
    /// the symptom this file exists to catch.
    pub fn detection(&mut self, detection: &Detection) {
        if !self.enabled() || detection.score == 0 {
            return;
        }
        let verdict = if detection.detected {
            "DETECTED"
        } else {
            "below   "
        };
        let hits = detection.hits.join(",");
        self.line(&format!(
            "{verdict} score={} hits=[{hits}]",
            detection.score
        ));
    }
}

impl Drop for DebugLog {
    fn drop(&mut self) {
        if self.enabled() {
            self.line("=== session ended ===");
        }
    }
}

/// `~/.isayes-debug.log`. A new name, not cry-aye's: both can be installed at
/// once during the port and two processes interleaving into one file produces
/// nonsense (D3).
fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut path = PathBuf::from(home);
    path.push(".isayes-debug.log");
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::{DebugLog, log_path};
    use crate::detector::Detection;

    /// Unset, it opens nothing and every call is inert. The default path must
    /// never touch the filesystem.
    #[test]
    fn disabled_by_default_and_inert() {
        // The env var is not set in the test process.
        let mut log = DebugLog::open();
        assert!(!log.enabled());
        log.line("this goes nowhere");
        log.detection(&Detection {
            detected: true,
            score: 9,
            hits: vec!["yes_no_buttons"],
        });
    }

    #[test]
    fn the_log_path_is_ours_not_cry_ayes() {
        let path = log_path().expect("HOME is set in a test process");
        assert!(path.ends_with(".isayes-debug.log"), "got {path:?}");
    }

    /// A score of zero is ordinary output; logging it would bury the signal.
    #[test]
    fn a_zero_score_is_not_worth_a_line() {
        let mut log = DebugLog::open();
        log.detection(&Detection {
            detected: false,
            score: 0,
            hits: Vec::new(),
        });
        assert!(!log.enabled());
    }
}
