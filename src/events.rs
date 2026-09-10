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

/// `claude`, running on a PTY.
pub struct Child {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Box<dyn Write + Send>,
    process: Box<dyn portable_pty::Child + Send + Sync>,
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
    /// `Instant::now()` lives in the threads below and nowhere else (D8).
    pub fn spawn(args: &[String], rows: u16, cols: u16) -> anyhow::Result<(Self, Receiver<Event>)> {
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
        spawn_signals(tx);

        Ok((
            Self {
                master,
                writer,
                process,
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
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Ok(master) = self.master.lock() {
            crate::ignore(master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }));
        }
    }

    /// Spec §7. Toggle the width by one column so Ink sees a dimension change
    /// and repaints — measured as the only thing that makes it repaint at all.
    /// The restore is delayed on a thread so the two `SIGWINCH`s arrive far
    /// enough apart not to be coalesced.
    pub fn force_redraw(&self, rows: u16, cols: u16) {
        if cols < 2 || rows < 1 {
            return;
        }
        self.resize(rows, cols - 1);
        let master = Arc::clone(&self.master);
        thread::spawn(move || {
            thread::sleep(REDRAW_HOLD);
            if let Ok(master) = master.lock() {
                crate::ignore(master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }));
            }
        });
    }

    /// Reap the child and return its exit code (§12).
    pub fn wait(&mut self) -> u8 {
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

fn spawn_reader(mut reader: Box<dyn Read + Send>, tx: Sender<Event>) {
    thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
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

fn spawn_signals(tx: Sender<Event>) {
    thread::spawn(move || {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
        let Ok(mut signals) =
            signal_hook::iterator::Signals::new([SIGWINCH, SIGINT, SIGTERM, SIGHUP])
        else {
            return;
        };
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
