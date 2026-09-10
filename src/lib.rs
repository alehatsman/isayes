//! `isayes` — run `claude` inside a PTY and answer its permission dialogs.
//!
//! The contract is `docs/spec.md`; the module boundaries are
//! `docs/architecture.md`. Everything the binary does lives here, so that each
//! piece can be tested without a terminal, a PTY, or a child process.

pub mod detector;
pub mod engine;
pub mod events;
