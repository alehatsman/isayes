//! `isayes` — run `claude` inside a PTY and answer its permission dialogs.
//!
//! The contract is `docs/spec.md`; the module boundaries are
//! `docs/architecture.md`. Everything the binary does lives here, so that each
//! piece can be tested without a terminal, a PTY, or a child process.

/// Deliberately discard a result there is genuinely nothing to do with.
///
/// The gate forbids `let _ =` on a `#[must_use]` value and forbids `.ok();`,
/// both for the same good reason: a dropped error is a silent failure. This
/// says the dropping is the decision, and makes every such site one `grep`
/// away.
///
/// Only two situations qualify. Inside `Drop`, where there is no caller to
/// tell and the terminal must still be handed back. And on a best-effort write
/// to a child that is already going away, where the loop is about to see `Eof`
/// and report the real outcome anyway.
pub fn ignore<T, E>(result: Result<T, E>) {
    drop(result);
}

pub mod detector;
pub mod engine;
pub mod events;
pub mod input;
pub mod terminal;
