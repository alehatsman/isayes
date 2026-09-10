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
