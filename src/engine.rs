//! All the state and all the decisions. Spec §5, §8–§11.
//!
//! The engine decides; the caller acts. [`Engine::handle`] takes one event and
//! returns the list of things that should happen — it never touches a file
//! descriptor, never spawns a thread, and never reads the clock (D8).
//!
//! That is what makes the suite at the bottom of this file fast and
//! deterministic: a test builds an engine, feeds it events stamped with
//! instants it invented, and asserts on the returned actions. Hand the engine a
//! writer "just for the approval" and every one of those tests needs a fake.

use std::time::{Duration, Instant};

use crate::detector;
use crate::events::Event;

/// Spec §5. Bytes past this are dropped from the front.
const BUFFER_CAP: usize = 10_000;

/// Spec §11. Silence this long makes the watchdog suspect a dialog is sitting
/// on screen, painted and emitting nothing.
const IDLE: Duration = Duration::from_secs(2);

/// Spec §11. A rescue redraw costs a repaint, so it is rate-limited.
const RESCUE_COOLDOWN: Duration = Duration::from_secs(3);

const FLASH: Duration = Duration::from_millis(800);
const FLASH_CANCEL: Duration = Duration::from_millis(500);
const FLASH_ERROR: Duration = Duration::from_secs(1);

/// Spec §9. `Ctrl+A`.
const CTRL_A: u8 = 0x01;
/// Spec §9. `Ctrl+Up` / `Ctrl+Down`.
const CTRL_UP: &[u8] = b"\x1b[1;5A";
const CTRL_DOWN: &[u8] = b"\x1b[1;5B";

/// Spec §3. `--delay` is clamped to this by the CLI; the keys respect it too.
const MAX_DELAY: u8 = 60;

/// Something the caller should do. Performed in the order returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these bytes to the PTY as an answer — `yes\r` or `\r`, one write,
    /// in that order (§8, §13 I7). A failed write comes back as
    /// [`Event::AnswerFailed`]; it is never retried (§13 I8).
    Answer(Vec<u8>),
    /// Write the user's own keystrokes to the PTY, verbatim (§9).
    Forward(Vec<u8>),
    /// Draw the status bar (§10).
    Status {
        /// Already-rendered text, no escapes.
        text: String,
        /// SGR parameter, e.g. `"33"`.
        colour: &'static str,
    },
    /// Make the child repaint (§7).
    ForceRedraw,
    /// Exit the process with this code (§12).
    Exit(u8),
}

/// A countdown that is running.
#[derive(Debug, Clone, Copy)]
struct Countdown {
    ends_at: Instant,
    /// Buffer length when the countdown began. Spec §8 — everything before it
    /// is the dialog being answered, everything after arrived while detection
    /// was switched off.
    watermark: usize,
}

/// A transient message that outranks the steady status text.
#[derive(Debug, Clone)]
struct Flash {
    text: String,
    colour: &'static str,
    until: Instant,
}

/// The wrapper's state machine. Spec §5, §8–§11.
#[derive(Debug)]
pub struct Engine {
    auto_approve: bool,
    delay: u8,
    /// Raw PTY bytes, not a `String`: a read can split a UTF-8 sequence and a
    /// PTY carries bytes that are not text at all. Detection sees a lossy
    /// conversion, which is harmless — every indicator in §6 is ASCII.
    buffer: Vec<u8>,
    countdown: Option<Countdown>,
    approvals: u32,
    flash: Option<Flash>,
    last_output: Instant,
    /// `None` until the first rescue. Seeding it with the start time would be
    /// claiming a rescue that never happened, and would hold the first real one
    /// off for the cooldown instead of the idle threshold.
    last_rescue: Option<Instant>,
}

impl Engine {
    /// `delay` in seconds, `started` as the origin for the idle timers.
    #[must_use]
    pub fn new(delay: u8, started: Instant) -> Self {
        Self {
            auto_approve: true,
            delay: delay.min(MAX_DELAY),
            buffer: Vec::new(),
            countdown: None,
            approvals: 0,
            flash: None,
            last_output: started,
            last_rescue: None,
        }
    }

    /// Feed one event, get back what should happen.
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Output(bytes, now) => self.on_output(&bytes, now),
            Event::Input(bytes, now) => self.on_input(&bytes, now),
            Event::Tick(now) => self.on_tick(now),
            Event::AnswerFailed(now) => {
                self.flash("✗ Failed to send approval", "31", now, FLASH_ERROR)
            }
            Event::Winch(now) => vec![self.status(now)],
            Event::Terminate(signo) => {
                let code = u8::try_from(128 + signo).unwrap_or(1);
                vec![Action::Exit(code)]
            }
            // The caller reaps the child and propagates its status (§12).
            Event::Eof => Vec::new(),
        }
    }

    /// The number of answers sent so far. For the debug log and the bar.
    #[must_use]
    pub fn approvals(&self) -> u32 {
        self.approvals
    }

    // ── events ───────────────────────────────────────────────────────────────

    fn on_output(&mut self, bytes: &[u8], now: Instant) -> Vec<Action> {
        self.last_output = now;
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > BUFFER_CAP {
            self.buffer.drain(..self.buffer.len() - BUFFER_CAP);
        }

        if self.auto_approve && self.countdown.is_none() && self.buffer_is_prompt() {
            return self.start_countdown(now);
        }
        Vec::new()
    }

    fn on_input(&mut self, bytes: &[u8], now: Instant) -> Vec<Action> {
        let Some(&first) = bytes.first() else {
            return Vec::new();
        };

        if bytes.len() == 1 && first == CTRL_A {
            return self.toggle(now);
        }

        if self.countdown.is_none() {
            if bytes.starts_with(CTRL_UP) {
                return self.change_delay(true, now);
            }
            if bytes.starts_with(CTRL_DOWN) {
                return self.change_delay(false, now);
            }
        }

        if self.countdown.is_some() {
            if first == b'\r' || first == b'\n' {
                return self.answer(now);
            }
            // Any other key cancels — and is swallowed, not forwarded (§9).
            // The buffer is left intact so the same dialog is re-detected:
            // cancel means "not yet", not "never" (§13 I9).
            self.countdown = None;
            return self.flash("✗ Cancelled", "90", now, FLASH_CANCEL);
        }

        vec![Action::Forward(bytes.to_vec())]
    }

    fn on_tick(&mut self, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();

        if self.countdown.is_some_and(|c| now >= c.ends_at) {
            actions.extend(self.answer(now));
        }
        actions.extend(self.watchdog(now));
        actions.push(self.status(now));
        actions
    }

    /// Spec §11. Two jobs, exclusive, and only while listening.
    fn watchdog(&mut self, now: Instant) -> Vec<Action> {
        if !self.auto_approve || self.countdown.is_some() {
            return Vec::new();
        }

        // A dialog that arrived while a countdown was running was never offered
        // to detection. This is the only thing that finds it.
        if !self.buffer.is_empty() && self.buffer_is_prompt() {
            return self.start_countdown(now);
        }

        // A dialog Claude has already painted emits no further bytes, so
        // nothing would re-enter detection on its own. Shake the tree.
        if now.duration_since(self.last_output) >= IDLE
            && self
                .last_rescue
                .is_none_or(|last| now.duration_since(last) >= RESCUE_COOLDOWN)
        {
            self.last_rescue = Some(now);
            return vec![Action::ForceRedraw];
        }

        Vec::new()
    }

    // ── answering ────────────────────────────────────────────────────────────

    fn start_countdown(&mut self, now: Instant) -> Vec<Action> {
        self.countdown = Some(Countdown {
            ends_at: now + Duration::from_secs(u64::from(self.delay)),
            watermark: self.buffer.len(),
        });
        // A zero delay answers in this same turn rather than waiting a tick.
        if self.delay == 0 {
            return self.answer(now);
        }
        Vec::new()
    }

    /// Spec §8, in the order written there.
    fn answer(&mut self, now: Instant) -> Vec<Action> {
        let watermark = self
            .countdown
            .take()
            .map_or(self.buffer.len(), |c| c.watermark);
        self.approvals += 1;

        // Decided from the *whole* buffer, before the truncation below.
        let wants_word = detector::needs_yes(&self.text());

        // Truncate, do not clear: what came after the watermark arrived while
        // detection was off and may be a second dialog (§8).
        let cut = watermark.min(self.buffer.len());
        self.buffer.drain(..cut);

        let bytes = if wants_word {
            b"yes\r".to_vec()
        } else {
            b"\r".to_vec()
        };
        let mut actions = vec![Action::Answer(bytes)];
        let message = format!("✓ Auto-approved (#{})", self.approvals);
        actions.extend(self.flash(&message, "32", now, FLASH));
        actions.push(Action::ForceRedraw);
        actions
    }

    // ── keys ─────────────────────────────────────────────────────────────────

    fn toggle(&mut self, now: Instant) -> Vec<Action> {
        self.countdown = None;
        self.auto_approve = !self.auto_approve;

        if !self.auto_approve {
            return self.flash("✗ Auto-approve DISABLED", "31", now, FLASH);
        }

        // Re-enabling with a dialog already on screen must not wait for the
        // next byte, which may never come. The countdown is started rather
        // than answered even at `delay == 0` — the tick that follows fires it,
        // and the deliberate keypress deserves one frame of "about to".
        if !self.buffer.is_empty() && self.buffer_is_prompt() {
            self.countdown = Some(Countdown {
                ends_at: now + Duration::from_secs(u64::from(self.delay)),
                watermark: self.buffer.len(),
            });
        }
        self.flash("✓ Auto-approve ENABLED", "32", now, FLASH)
    }

    fn change_delay(&mut self, increase: bool, now: Instant) -> Vec<Action> {
        let old = self.delay;
        if increase {
            self.delay = self.delay.saturating_add(1).min(MAX_DELAY);
        } else {
            self.delay = self.delay.saturating_sub(1);
        }
        if old == self.delay {
            // At the cap or the floor the key is silent (§10).
            return Vec::new();
        }
        let message = format!("⏱  Delay: {old}s → {}s", self.delay);
        self.flash(&message, "36", now, FLASH)
    }

    // ── status ───────────────────────────────────────────────────────────────

    fn flash(
        &mut self,
        text: &str,
        colour: &'static str,
        now: Instant,
        hold: Duration,
    ) -> Vec<Action> {
        self.flash = Some(Flash {
            text: text.to_owned(),
            colour,
            until: now + hold,
        });
        vec![self.status(now)]
    }

    /// Spec §10, highest priority first.
    fn status(&self, now: Instant) -> Action {
        if let Some(flash) = &self.flash
            && now < flash.until
        {
            return Action::Status {
                text: flash.text.clone(),
                colour: flash.colour,
            };
        }

        if let Some(countdown) = self.countdown {
            let left = countdown.ends_at.saturating_duration_since(now);
            // Rounded up, so a 3 s delay reads 3, 2, 1 and never 0 twice.
            let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
            return Action::Status {
                text: format!(
                    "⏱  Auto-approving in {secs}s... (Enter=now, any key=cancel, Ctrl+A=off)"
                ),
                colour: "33",
            };
        }

        if self.auto_approve {
            Action::Status {
                text: format!(
                    "auto-approve ON  {} approved  delay {}s  [Ctrl+A=toggle, Ctrl+↑↓=delay]",
                    self.approvals, self.delay
                ),
                colour: "2",
            }
        } else {
            Action::Status {
                text: format!(
                    "auto-approve OFF  delay {}s  [Ctrl+A=toggle, Ctrl+↑↓=delay]",
                    self.delay
                ),
                colour: "90",
            }
        }
    }

    // ── buffer ───────────────────────────────────────────────────────────────

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }

    fn buffer_is_prompt(&self) -> bool {
        detector::is_prompt(&self.text()).detected
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, CTRL_A, CTRL_DOWN, CTRL_UP, Engine};
    use crate::events::Event;
    use std::time::{Duration, Instant};

    /// The corpus's `loop harness canonical` entry, verbatim. Both suites use
    /// the same bytes so they cannot drift.
    const DIALOG: &str = " 1. Yes\n 2. No\n Enter to approve \n Esc to cancel\n";

    /// Tests may read the clock; the engine may not (D8). This is the only
    /// `Instant::now()` in the crate outside `events.rs`, and it is an origin —
    /// every instant after it is arithmetic, so nothing here waits on anything.
    fn origin() -> Instant {
        Instant::now()
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn answers(actions: &[Action]) -> Vec<Vec<u8>> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Answer(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .collect()
    }

    fn n_answers(actions: &[Action]) -> usize {
        answers(actions).len()
    }

    /// Run `count` ticks 200 ms apart starting at `from_ms`, collecting actions.
    fn ticks(engine: &mut Engine, t0: Instant, from_ms: u64, count: u64) -> Vec<Action> {
        (0..count)
            .flat_map(|i| engine.handle(Event::Tick(at(t0, from_ms + i * 200))))
            .collect()
    }

    fn out(engine: &mut Engine, t0: Instant, ms: u64, text: &str) -> Vec<Action> {
        engine.handle(Event::Output(text.as_bytes().to_vec(), at(t0, ms)))
    }

    // ── I1 ───────────────────────────────────────────────────────────────────

    /// Output is never modified, reordered, or delayed by detection: the caller
    /// has already written it, and plain output produces no action at all.
    #[test]
    fn i1_the_engine_does_not_sit_in_the_output_path() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        let actions = out(&mut e, t0, 1, "compiling 1 of 40\nhello world\n");
        assert!(actions.is_empty(), "acted on plain output: {actions:?}");
    }

    // ── I2 ───────────────────────────────────────────────────────────────────

    /// One dialog, one answer — however many ticks follow. This is the
    /// watermark doing its job (§8); without it the answered dialog stays in
    /// the buffer and is re-detected forever.
    #[test]
    fn i2_one_dialog_is_answered_exactly_once() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        assert_eq!(n_answers(&out(&mut e, t0, 1, DIALOG)), 1);

        let later = ticks(&mut e, t0, 200, 50);
        assert_eq!(n_answers(&later), 0, "the watermark did not hold");
        assert_eq!(e.approvals(), 1);
    }

    // ── I3 ───────────────────────────────────────────────────────────────────

    #[test]
    fn i3_nothing_is_answered_while_auto_approve_is_off() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        e.handle(Event::Input(vec![CTRL_A], at(t0, 1)));

        assert_eq!(n_answers(&out(&mut e, t0, 2, DIALOG)), 0);
        assert_eq!(n_answers(&ticks(&mut e, t0, 200, 50)), 0);
        assert_eq!(e.approvals(), 0);
    }

    // ── I4 ───────────────────────────────────────────────────────────────────

    /// The buffer is cumulative, so a dialog delivered in two reads scores the
    /// same as one. A PTY splits wherever it likes.
    #[test]
    fn i4_a_dialog_split_across_two_reads_still_answers_once() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        // Split right before the "No" button: the yes/no pair scores 5 only
        // when both halves are present, so the head alone must score nothing.
        let split = DIALOG
            .find("2.")
            .expect("the canonical dialog has two buttons");
        let (head, tail) = DIALOG.split_at(split);
        assert_eq!(
            n_answers(&out(&mut e, t0, 1, head)),
            0,
            "half a dialog answered"
        );
        assert_eq!(n_answers(&out(&mut e, t0, 2, tail)), 1);
        assert_eq!(n_answers(&ticks(&mut e, t0, 200, 20)), 0);
    }

    // ── I5 ───────────────────────────────────────────────────────────────────

    /// Volume before a dialog must not push it out of the 50-line tail.
    #[test]
    fn i5_noise_before_a_dialog_does_not_blind_detection() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        let noise = "x".repeat(5_000);
        assert_eq!(n_answers(&out(&mut e, t0, 1, &noise)), 0);
        assert_eq!(n_answers(&out(&mut e, t0, 2, DIALOG)), 1);
    }

    // ── I6 ───────────────────────────────────────────────────────────────────

    /// Representative plain output. The full false-positive corpus is the
    /// detector's own suite (`tests/fixtures/detector.toml`); what this pins is
    /// that the engine sends nothing when the detector says no.
    #[test]
    fn i6_plain_output_is_never_answered() {
        let t0 = origin();
        for (i, text) in [
            "Here is the code:\n```go\nfunc foo() {}\n```\n",
            "// This is a comment\n// with multiple lines\n",
            "Running: npm install\nfetching packages...\ndone\n",
            "Here's how to implement the feature. Do you want me to explain more?",
        ]
        .iter()
        .enumerate()
        {
            let mut e = Engine::new(0, t0);
            let actions = out(&mut e, t0, 1, text);
            assert_eq!(
                n_answers(&actions),
                0,
                "false positive on case {i}: {text:?}"
            );
            assert_eq!(
                n_answers(&ticks(&mut e, t0, 200, 20)),
                0,
                "case {i} on a tick"
            );
        }
    }

    // ── I7 ───────────────────────────────────────────────────────────────────

    /// A dialog that wants the word gets `yes\r`, in one write, in that order.
    /// Two writes would let the child see a bare CR first and submit an empty
    /// answer.
    #[test]
    fn i7_a_word_dialog_is_answered_with_yes_then_cr_in_one_write() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        let actions = out(&mut e, t0, 1, "Do you want to proceed? (y/n)");
        assert_eq!(answers(&actions), vec![b"yes\r".to_vec()]);
    }

    #[test]
    fn i7_a_button_dialog_is_answered_with_a_bare_cr() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        let actions = out(&mut e, t0, 1, DIALOG);
        assert_eq!(answers(&actions), vec![b"\r".to_vec()]);
    }

    // ── I8 ───────────────────────────────────────────────────────────────────

    /// A failed PTY write flashes and returns. It never retries — the loop must
    /// not spin on a dead child — and it must still serve the next event.
    #[test]
    fn i8_a_failed_write_does_not_retry_and_the_loop_keeps_serving() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        assert_eq!(n_answers(&out(&mut e, t0, 1, DIALOG)), 1);

        let failed = e.handle(Event::AnswerFailed(at(t0, 2)));
        assert_eq!(n_answers(&failed), 0, "retried a failed write");
        assert!(matches!(
            failed.as_slice(),
            [Action::Status { colour: "31", .. }]
        ));

        // Still alive: the next tick is served and still draws a bar.
        let next = e.handle(Event::Tick(at(t0, 200)));
        assert!(next.iter().any(|a| matches!(a, Action::Status { .. })));
    }

    // ── I9 ───────────────────────────────────────────────────────────────────

    /// Cancel is "not yet", not "never": the buffer survives, so the watchdog
    /// re-detects the same dialog and the countdown starts again.
    #[test]
    fn i9_a_cancelled_countdown_re_detects() {
        let t0 = origin();
        let mut e = Engine::new(5, t0);

        assert_eq!(
            n_answers(&out(&mut e, t0, 1, DIALOG)),
            0,
            "delay 5 answered at once"
        );

        let cancelled = e.handle(Event::Input(b"x".to_vec(), at(t0, 50)));
        assert!(matches!(
            cancelled.as_slice(),
            [Action::Status { colour: "90", .. }]
        ));
        assert!(
            !cancelled.iter().any(|a| matches!(a, Action::Forward(_))),
            "the cancelling keystroke was forwarded to the child"
        );

        // Watchdog picks it up again, and it answers five seconds after that.
        assert_eq!(n_answers(&ticks(&mut e, t0, 200, 2)), 0);
        assert_eq!(n_answers(&ticks(&mut e, t0, 5_600, 2)), 1);
    }

    // ── I10 ──────────────────────────────────────────────────────────────────

    /// A dialog that arrives after the previous one was answered gets its own
    /// answer. The watermark narrows the buffer; it does not deafen the tool.
    #[test]
    fn i10_sequential_dialogs_each_get_their_own_answer() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        assert_eq!(n_answers(&out(&mut e, t0, 1, DIALOG)), 1);
        assert_eq!(n_answers(&out(&mut e, t0, 100, DIALOG)), 1);
        assert_eq!(n_answers(&out(&mut e, t0, 200, DIALOG)), 1);
        assert_eq!(e.approvals(), 3);
    }

    // ── I11 ──────────────────────────────────────────────────────────────────

    /// Dialogs arriving faster than the countdown may yield fewer answers than
    /// dialogs. That is correct, not a defect — the redraw after each answer
    /// re-surfaces whatever is still pending. What is guaranteed: at least one
    /// answer, and a loop that is still serving events afterwards.
    ///
    /// cry-aye's equivalent asserts the same floor, and sleeps three seconds to
    /// do it.
    #[test]
    fn i11_overlapping_dialogs_coalesce_and_never_deadlock() {
        let t0 = origin();
        let mut e = Engine::new(1, t0);

        let mut sent = 0;
        for i in 0..8 {
            sent += n_answers(&out(&mut e, t0, 1 + i * 20, DIALOG));
        }
        sent += n_answers(&ticks(&mut e, t0, 200, 50));

        assert!(sent >= 1, "eight dialogs produced no answer at all");
        assert!(
            sent <= 8,
            "answered {sent} times for eight dialogs — the watermark is leaking"
        );

        let still_alive = e.handle(Event::Tick(at(t0, 20_000)));
        assert!(
            still_alive
                .iter()
                .any(|a| matches!(a, Action::Status { .. }))
        );
    }

    // ── keys, §9 ─────────────────────────────────────────────────────────────

    #[test]
    fn ordinary_keystrokes_are_forwarded_verbatim() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        let actions = e.handle(Event::Input(b"ls -la\r".to_vec(), at(t0, 1)));
        assert_eq!(actions, vec![Action::Forward(b"ls -la\r".to_vec())]);
    }

    #[test]
    fn an_empty_read_does_nothing() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        assert!(e.handle(Event::Input(Vec::new(), at(t0, 1))).is_empty());
    }

    #[test]
    fn ctrl_a_is_never_forwarded() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        let actions = e.handle(Event::Input(vec![CTRL_A], at(t0, 1)));
        assert!(!actions.iter().any(|a| matches!(a, Action::Forward(_))));
    }

    #[test]
    fn enter_during_a_countdown_answers_now() {
        let t0 = origin();
        let mut e = Engine::new(60, t0);
        assert_eq!(n_answers(&out(&mut e, t0, 1, DIALOG)), 0);

        let actions = e.handle(Event::Input(b"\r".to_vec(), at(t0, 50)));
        assert_eq!(n_answers(&actions), 1, "Enter did not answer immediately");
    }

    #[test]
    fn ctrl_up_and_down_move_the_delay_and_are_silent_at_the_bounds() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        // Floor: already 0, so nothing happens and nothing is drawn.
        assert!(
            e.handle(Event::Input(CTRL_DOWN.to_vec(), at(t0, 1)))
                .is_empty()
        );

        let up = e.handle(Event::Input(CTRL_UP.to_vec(), at(t0, 2)));
        assert!(matches!(
            up.as_slice(),
            [Action::Status { colour: "36", .. }]
        ));

        // And they are not forwarded to the child.
        assert!(!up.iter().any(|a| matches!(a, Action::Forward(_))));
    }

    #[test]
    fn the_delay_keys_are_ignored_while_a_countdown_runs() {
        let t0 = origin();
        let mut e = Engine::new(5, t0);
        out(&mut e, t0, 1, DIALOG);

        // Spec §9: during a countdown, any key that is not Enter cancels.
        let actions = e.handle(Event::Input(CTRL_UP.to_vec(), at(t0, 50)));
        assert!(matches!(
            actions.as_slice(),
            [Action::Status { colour: "90", .. }]
        ));
    }

    #[test]
    fn toggling_off_then_on_with_a_dialog_waiting_starts_a_countdown() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        e.handle(Event::Input(vec![CTRL_A], at(t0, 1)));
        assert_eq!(n_answers(&out(&mut e, t0, 2, DIALOG)), 0);

        // Re-enabling arms it; the next tick fires it.
        assert_eq!(
            n_answers(&e.handle(Event::Input(vec![CTRL_A], at(t0, 3)))),
            0
        );
        assert_eq!(n_answers(&ticks(&mut e, t0, 200, 1)), 1);
    }

    // ── status, §10 ──────────────────────────────────────────────────────────

    #[test]
    fn the_countdown_rounds_seconds_up() {
        let t0 = origin();
        let mut e = Engine::new(3, t0);
        out(&mut e, t0, 0, DIALOG);

        // 2.4 s left reads as 3, not 2: the bar never shows the same number
        // twice or reaches 0 before firing.
        let actions = ticks(&mut e, t0, 600, 1);
        let text = actions
            .iter()
            .find_map(|a| match a {
                Action::Status { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a tick always draws the bar");
        assert!(text.contains("in 3s"), "got {text:?}");
    }

    #[test]
    fn the_bar_says_off_when_it_is_off() {
        let t0 = origin();
        let mut e = Engine::new(7, t0);
        e.handle(Event::Input(vec![CTRL_A], at(t0, 1)));

        // Past the flash, the steady text returns.
        let actions = ticks(&mut e, t0, 2_000, 1);
        let text = actions
            .iter()
            .find_map(|a| match a {
                Action::Status { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("a tick always draws the bar");
        assert_eq!(
            text,
            "auto-approve OFF  delay 7s  [Ctrl+A=toggle, Ctrl+↑↓=delay]"
        );
    }

    // ── watchdog, §11 ────────────────────────────────────────────────────────

    /// A painted dialog emits no further bytes, so nothing re-enters detection
    /// on its own. After two seconds of silence the engine shakes the tree.
    #[test]
    fn silence_triggers_one_rescue_redraw_and_then_backs_off() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);

        let quiet = ticks(&mut e, t0, 200, 10); // 0.2 s .. 2.0 s
        let redraws = quiet
            .iter()
            .filter(|a| matches!(a, Action::ForceRedraw))
            .count();
        assert_eq!(redraws, 1, "expected exactly one rescue in the first 2 s");

        // The cooldown holds the next one off for three seconds.
        let soon = ticks(&mut e, t0, 2_200, 5);
        assert_eq!(
            soon.iter()
                .filter(|a| matches!(a, Action::ForceRedraw))
                .count(),
            0
        );
    }

    // ── exit, §12 ────────────────────────────────────────────────────────────

    #[test]
    fn a_fatal_signal_exits_with_128_plus_the_signal() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        assert_eq!(e.handle(Event::Terminate(2)), vec![Action::Exit(130)]);
    }

    /// The caller reaps the child and propagates its status, so the engine has
    /// nothing to add.
    #[test]
    fn eof_is_the_callers_business() {
        let t0 = origin();
        let mut e = Engine::new(0, t0);
        assert!(e.handle(Event::Eof).is_empty());
    }
}
