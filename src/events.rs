//! What the outside world tells the engine. Spec §4.
//!
//! The enum lives here; the threads that produce it arrive with phase 1. It is
//! split that way because the engine is the only consumer and it must be
//! buildable — and testable — before a PTY exists.
//!
//! Every variant that the engine makes a timing decision on carries the instant
//! it happened. That is D8, and it is the reason the test suite has no sleeps
//! in it: `Instant::now()` belongs to the producers, never to the loop.

use std::time::Instant;

/// One thing that happened, stamped with when.
#[derive(Debug, Clone)]
pub enum Event {
    /// Bytes read from the PTY. Already written to stdout by the caller —
    /// the engine sees a copy and never sits in the output path (§13 I1).
    Output(Vec<u8>, Instant),
    /// Bytes read from the user's stdin, before any forwarding.
    Input(Vec<u8>, Instant),
    /// The 200 ms heartbeat. §11.
    Tick(Instant),
    /// The PTY write for an [`crate::engine::Action::Answer`] failed. §13 I8.
    AnswerFailed(Instant),
    /// The terminal was resized. The resize itself is `terminal.rs`'s job;
    /// the engine only redraws.
    Winch(Instant),
    /// A fatal signal. The engine turns it into an exit code.
    Terminate(i32),
    /// The PTY closed — `claude` is gone. The engine has nothing to say; the
    /// caller reaps the child and propagates its status (§12).
    Eof,
}

impl Event {
    /// When it happened, for the variants that carry it.
    ///
    /// This is how `main` gets an instant without reading the clock: the
    /// producers stamp, everyone downstream reads the stamp (D8).
    #[must_use]
    pub fn stamp(&self) -> Option<Instant> {
        match *self {
            Self::Output(_, now)
            | Self::Input(_, now)
            | Self::Tick(now)
            | Self::AnswerFailed(now)
            | Self::Winch(now) => Some(now),
            Self::Terminate(_) | Self::Eof => None,
        }
    }
}

// ── The child and the producer threads (task 1.1) ────────────────────────────

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Spec §4. The heartbeat that drives countdowns and the watchdog.
pub const TICK: Duration = Duration::from_millis(200);

/// Spec §7. How long the PTY stays one column narrow during a `force_redraw`
/// — long enough that the kernel cannot coalesce the two `SIGWINCH`s, which is
/// the whole reason the toggle works.
pub const REDRAW_HOLD: Duration = Duration::from_millis(50);

const READ_BUF: usize = 4096;

/// How long [`Child::wait`] gives the child to exit on its own before killing
/// it. Spec §12, §13 I13.
///
/// The wait cannot be unbounded. `Event::Eof` also stands for "the PTY read
/// failed", and a read can fail while the child is perfectly healthy — at
/// which point nothing is draining its output, it blocks on its next write,
/// and a blocking `wait()` never returns. A hung wrapper leaves the terminal
/// in raw mode with no bar and no keys, which is the worst way to fail.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// How often [`Child::wait`] checks during that grace period.
const REAP_POLL: Duration = Duration::from_millis(20);

/// The signal handlers, registered. Installing them is separate from consuming
/// them so registration can happen *before* raw mode and the scroll region are
/// on: between those two points the default disposition is still in force, and
/// a `SIGTERM` there — a job-control kill, a supervisor, a `SIGHUP` on
/// terminal close — kills the process with no `Drop` and leaves the user's
/// shell raw and boxed in (§13 I13).
pub struct Signals(signal_hook::iterator::Signals);

impl std::fmt::Debug for Signals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signals")
    }
}

/// Register the handlers. Call this first, before anything is acquired.
pub fn install_signals() -> anyhow::Result<Signals> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
    Ok(Signals(signal_hook::iterator::Signals::new([
        SIGWINCH, SIGINT, SIGTERM, SIGHUP,
    ])?))
}

/// `claude`, running on a PTY.
pub struct Child {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Box<dyn Write + Send>,
    process: Box<dyn portable_pty::Child + Send + Sync>,
    /// The size the child is *supposed* to have. A `force_redraw` narrows the
    /// PTY behind this value's back and the restore thread reads it, so a
    /// `SIGWINCH` arriving mid-redraw wins instead of being undone.
    size: Arc<Mutex<PtySize>>,
    /// Wakes the restore thread. One thread for the process, not one per
    /// redraw — the watchdog fires these on a timer.
    restore: Sender<()>,
}

impl std::fmt::Debug for Child {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Child")
    }
}

impl Child {
    /// Spawn `claude` with `args` on a PTY of this size, and start every
    /// producer thread. Spec §4 steps 1–2 and 5.
    ///
    /// `signals` comes from [`install_signals`], called before the terminal
    /// was acquired — see [`Signals`] for why the registration cannot wait
    /// until here.
    ///
    /// `Instant::now()` lives in the threads below and nowhere else (D8).
    pub fn spawn(
        args: &[String],
        rows: u16,
        cols: u16,
        signals: Signals,
    ) -> anyhow::Result<(Self, Receiver<Event>)> {
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new("claude");
        for arg in args {
            cmd.arg(arg);
        }
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        let process = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = Arc::new(Mutex::new(pair.master));

        let (tx, rx) = channel();
        spawn_reader(reader, tx.clone());
        spawn_stdin(tx.clone());
        spawn_ticker(tx.clone());
        spawn_signals(signals, tx);

        let size = Arc::new(Mutex::new(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }));
        let (restore, restore_rx) = channel();
        spawn_restore(Arc::clone(&master), Arc::clone(&size), restore_rx);

        Ok((
            Self {
                master,
                writer,
                process,
                size,
                restore,
            },
            rx,
        ))
    }

    /// Write to the child. A failure is reported as [`Event::AnswerFailed`] by
    /// the caller and never retried (§13 I8).
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Tell the child how much room it has. Advisory — the scroll region is
    /// what actually keeps it off our rows (§7).
    ///
    /// This is what "the size" means from here on: a later restore reads it
    /// back, so a resize during a redraw is not undone by one.
    pub fn resize(&self, rows: u16, cols: u16) {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Ok(mut current) = self.size.lock() {
            *current = size;
        }
        apply(&self.master, size);
    }

    /// Spec §7. Toggle the width by one column so Ink sees a dimension change
    /// and repaints — measured as the only thing that makes it repaint at all.
    /// The restore is delayed so the two `SIGWINCH`s arrive far enough apart
    /// not to be coalesced.
    ///
    /// The narrowing deliberately does **not** go through
    /// [`resize`](Self::resize): it is a flicker, not a new size, and the
    /// restore must put back whatever the size is *then* rather than whatever
    /// it was when the redraw started.
    pub fn force_redraw(&self, rows: u16, cols: u16) {
        if cols < 2 || rows < 1 {
            return;
        }
        apply(
            &self.master,
            PtySize {
                rows,
                cols: cols - 1,
                pixel_width: 0,
                pixel_height: 0,
            },
        );
        crate::ignore(self.restore.send(()));
    }

    /// Reap the child and return its exit code (§12).
    ///
    /// Bounded by a two-second grace. A child that has not gone within it is
    /// killed rather than waited on forever.
    pub fn wait(&mut self) -> u8 {
        let mut waited = Duration::ZERO;
        while waited < EXIT_GRACE {
            match self.process.try_wait() {
                Ok(Some(status)) => return u8::try_from(status.exit_code()).unwrap_or(1),
                Ok(None) => {}
                Err(_) => return 1,
            }
            thread::sleep(REAP_POLL);
            waited += REAP_POLL;
        }
        // Still running with its PTY no longer being read: it will block on
        // its next write and never exit on its own.
        self.kill();
        match self.process.wait() {
            Ok(status) => u8::try_from(status.exit_code()).unwrap_or(1),
            Err(_) => 1,
        }
    }

    /// Stop the child. Used on the signal path, where we exit first.
    pub fn kill(&mut self) {
        crate::ignore(self.process.kill());
    }
}

/// Push a size onto the PTY. Advisory and best-effort, like every resize here.
fn apply(master: &Arc<Mutex<Box<dyn MasterPty + Send>>>, size: PtySize) {
    if let Ok(master) = master.lock() {
        crate::ignore(master.resize(size));
    }
}

/// One thread, woken by [`Child::force_redraw`], that puts the width back
/// after [`REDRAW_HOLD`] — reading the size at that moment, not the one the
/// redraw captured.
fn spawn_restore(
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    size: Arc<Mutex<PtySize>>,
    rx: Receiver<()>,
) {
    thread::spawn(move || {
        while rx.recv().is_ok() {
            thread::sleep(REDRAW_HOLD);
            // Drain any redraws that piled up during the hold: they all want
            // the same thing, and it is about to happen once.
            while rx.try_recv().is_ok() {}
            let current = {
                let Ok(guard) = size.lock() else { return };
                *guard
            };
            apply(&master, current);
        }
    });
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, tx: Sender<Event>) {
    thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                // A signal landing mid-read is not the child going away.
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                // `Ok(0)` is EOF; a hard error on the master means the same
                // thing in practice, and `wait` is bounded in case it does
                // not (see EXIT_GRACE).
                Ok(0) | Err(_) => {
                    crate::ignore(tx.send(Event::Eof));
                    return;
                }
                Ok(n) => {
                    let chunk = buf.get(..n).unwrap_or_default().to_vec();
                    if tx.send(Event::Output(chunk, Instant::now())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

fn spawn_stdin(tx: Sender<Event>) {
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; READ_BUF];
        loop {
            match stdin.read(&mut buf) {
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let chunk = buf.get(..n).unwrap_or_default().to_vec();
                    if tx.send(Event::Input(chunk, Instant::now())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

fn spawn_ticker(tx: Sender<Event>) {
    thread::spawn(move || {
        loop {
            thread::sleep(TICK);
            if tx.send(Event::Tick(Instant::now())).is_err() {
                return;
            }
        }
    });
}

fn spawn_signals(signals: Signals, tx: Sender<Event>) {
    thread::spawn(move || {
        use signal_hook::consts::SIGWINCH;
        let mut signals = signals.0;
        for signal in &mut signals {
            let event = if signal == SIGWINCH {
                Event::Winch(Instant::now())
            } else {
                Event::Terminate(signal)
            };
            if tx.send(event).is_err() {
                return;
            }
        }
    });
}
