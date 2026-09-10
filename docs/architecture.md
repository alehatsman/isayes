# isayes — architecture

Status: v0.1 · 2026-09-10 · the seams, fixed before anyone writes code.

[spec.md](spec.md) says what the tool does. This says where each part of it
lives and what the boundaries between those parts are. It exists so that three
agents building three phases at once produce code that fits together instead of
three good designs that do not.

**These signatures are the contract between phases.** Change one and say so in
the same commit; do not quietly widen it.

## Module map

| File | Owns | Spec | Depends on |
|---|---|---|---|
| `lib.rs` | Nothing but `pub mod` lines and the crate docs. | — | — |
| `main.rs` | CLI, wiring, exit codes. No logic. | §3, §12 | everything |
| `detector.rs` | Scoring output. Pure — no I/O, no state, no clock. | §6 | nothing |
| `terminal.rs` | The real terminal and the PTY's size. Raw mode, the margin, the bar, teardown. | §7 | nothing |
| `events.rs` | The producer threads and the `Event` enum. The only place `Instant::now()` is called. | §4 | `terminal.rs` |
| `input.rs` | Splitting stdin into units and classifying them. Pure. | §9, D11 | nothing |
| `engine.rs` | All the state and all the decisions. Pure — no I/O, no clock. | §5, §8–§11 | `detector.rs`, `input.rs` |
| `debug.rs` | The debug log. | §15 | nothing |

Eight files. `input.rs` is the eighth, and this paragraph is the conversation
the previous version of it asked for: it is not `engine.rs` outgrowing itself,
it is scope the 1.0 measurement created (D11). Stdin has to be split into
complete units and classified before §9's key table means anything, and that is
a byte-level state machine — the same one `terminal.rs` already runs in the
other direction. Putting it in `engine.rs` would have put a parser inside the
state machine that consumes it.

If a ninth appears, same rule.

`engine.rs` is not called `loop.rs` because `loop` is a keyword.

**Lib plus bin, and it is not ceremony** (D10). In a bin-only crate every
module `main.rs` has not wired up yet is `dead_code`, which the gate treats as
an error — so each phase would ship an `#[expect(dead_code)]` that
`unfulfilled_lint_expectations` then fires on under `--all-targets`, because
the tests *do* use the code. A library target makes the modules genuinely
reachable, so phases land clean and in any order.

The lib also means `pub`, not `pub(crate)`, is the right visibility for a
module's surface — and `missing_docs` therefore bites, which is the point.

## The one idea

**The engine decides, `main` acts.** The engine is a state machine over events
that returns a list of actions and performs none of them. It never touches a
file descriptor, never reads a clock, never allocates a thread.

```rust
pub fn handle(&mut self, event: Event) -> Vec<Action>
```

That is the whole reason the test suite in §14 is fast and cannot flake: a test
constructs an `Engine`, feeds it events with instants it made up, and asserts
on the `Vec<Action>` it gets back. No PTY, no terminal, no mock, no trait
object, no sleeps. Every invariant I1–I11 is reachable this way.

The temptation is to hand the engine a writer "just for the approval". Do not.
The moment it can write, the tests need a fake writer, and the fake writer is
how the Go suite ended up asserting "at least one approval eventually".

## The seams

### `detector.rs` — phase 2, depends on nothing

```rust
pub struct Detection {
    pub detected: bool,          // score >= 3
    pub score: u32,
    pub hits: Vec<&'static str>, // indicator names, for the debug log (§15)
}

pub fn strip_ansi(text: &str) -> String;
pub fn is_prompt(text: &str) -> Detection;
pub fn needs_yes(text: &str) -> bool;
```

`hits` is not decoration. It is the whole mitigation for plan.md's known
liability: when Claude Code moves a string, the debug log has to say which
indicator stopped matching.

Tests are a loop over `tests/fixtures/detector.toml` (D9). Twenty-seven cases,
already verified to hold against §6 as written.

### `terminal.rs` — phase 1, depends on nothing

```rust
pub const STATUS_ROWS: u16 = 1;

pub struct Terminal { /* … */ }

impl Terminal {
    /// Raw mode, size, PTY winsize, margin, screen clear. Fails the process.
    pub fn acquire(pty: &Pty) -> Result<Self, Error>;
    pub fn size(&self) -> (u16, u16);
    /// SIGWINCH: re-read size, resize the PTY, re-apply the margin. §7.
    pub fn resize(&mut self) -> Result<(), Error>;
    /// stdout, byte for byte, then the re-margin scan. Never rewrites.
    pub fn pass_through(&mut self, bytes: &[u8]) -> io::Result<()>;
    pub fn draw_status(&mut self, text: &str, colour: &str);
    pub fn force_redraw(&mut self);
}

impl Drop for Terminal { /* full-height region, clear bar, restore termios */ }
```

Two pure functions carry the parts worth testing, so I12 needs no terminal:

```rust
/// The DECSTBM sequence for a terminal this tall.
pub fn margin(height: u16) -> Vec<u8>;
/// DECSTBM restoring the full height. Teardown writes it. §13 I13.
pub const RESET_MARGIN: &[u8];

/// Watches child output for anything that clears the scroll region. §7.
pub struct MarginWatch { /* … */ }
impl MarginWatch {
    pub fn new() -> Self;
    pub fn feed(&mut self, chunk: &[u8]) -> bool;
}
```

`needs_remargin` was drafted as a free function. It cannot be one: an escape
sequence splits across PTY reads at any byte, so the carry has to live
somewhere. It is a three-state machine over bytes — ground, escape, CSI — and the machine
*is* the carry, so no lookback buffer is needed and a sequence delivered one
byte at a time is still recognised. There is deliberately no string/OSC state;
`terminal.rs` documents what that costs.

Comparing the CSI parameters matters: `?1049h` is the alternate screen,
`?1000h` is mouse reporting, and `p` is only a soft reset with the `!`
intermediate. A `contains` over the chunk gets all three wrong.

### `events.rs` — phase 1

```rust
pub enum Event {
    Output(Vec<u8>, Instant),
    Input(Vec<u8>, Instant),
    Tick(Instant),
    /// The PTY write for an answer failed. §13 I8.
    AnswerFailed(Instant),
    Winch(Instant),
    Terminate(i32),
    Eof,
}

pub fn spawn(pty: &Pty) -> Receiver<Event>;   // phase 1
```

**The enum landed with phase 3, `spawn` lands with phase 1.** The engine is the
only consumer and had to be buildable before a PTY existed. `Winch` carries an
instant because the engine redraws the bar on it, and the bar's contents are
time-dependent.

`Instant::now()` lives here and in no other *non-test* file (D8). A reviewer
running `grep -rn 'Instant::now' src/` sees `events.rs`, plus a single origin
helper in `engine.rs`'s test module — every instant in a test is arithmetic on
that origin, so nothing waits on anything.

### `engine.rs` — phase 3, depends on `detector.rs` only

```rust
pub enum Action {
    /// Write these bytes to the PTY. `yes\r` or `\r`, one write. §8.
    Answer(Vec<u8>),
    /// Write the user's own keystrokes to the PTY, verbatim. §9.
    Forward(Vec<u8>),
    Status { text: String, colour: &'static str },
    ForceRedraw,
    Exit(u8),
}

pub struct Engine { /* buffer, watermark, countdown, auto_approve, counts */ }

impl Engine {
    pub fn new(delay: u8, started: Instant) -> Self;
    pub fn handle(&mut self, event: Event) -> Vec<Action>;
    pub fn approvals(&self) -> u32;
}
```

`Forward` was missing from the first draft of this file. Forwarding a keystroke
is a PTY write like an answer is, but only one of the two counts as an approval
or raises a flash, and a test that cannot tell them apart cannot assert §9's
"consumed keys are never forwarded".

`main` performs the actions in order. An `Answer` whose write fails comes back
as `Event::AnswerFailed` on the next turn of the loop — that is the round trip
that makes I8 an assertion instead of a three-second sleep.

## Who can build what, at the same time

```
        architecture.md (this file — done)
                 │
      ┌──────────┴───────────┐
      │                      │
  phase 1                phase 2
  terminal.rs            detector.rs
  events.rs              (pure, zero deps)
      │                      │
      └──────────┬───────────┘
                 │
             phase 3
             engine.rs  — needs only detector's signature,
                          so it can start against a stub
                 │
             phase 4
             main.rs wiring, cut-over
```

Phase 2 touches no file phase 1 touches. They are genuinely parallel — one
worktree each, no coordination beyond this file.

Phase 3 needs `detector::is_prompt` to *exist*, not to be finished. If it
starts early it starts against a stub that returns the corpus's canonical
answer, and swaps to the real one when phase 2 lands.

## Rules that are not negotiable

1. **The engine gets no I/O and no clock.** Both are one careless line away and
   both cost the test suite (D8).
2. **`unwrap` and `panic` are gate failures** in non-test code, and `todo!()`
   is one too — a stub that compiles green is worse than a red build. Return
   an error or handle it.
3. **`clippy.toml`, `rustfmt.toml`, `deny.toml`, `.cargo/config.toml` are not
   yours.** They are copies from rust-quality. Fix upstream (D6).
4. **Cross-reference the spec.** A comment that says *why* points at a section
   or a decision. If the code and the spec disagree, one of them is a bug —
   say which, in the commit.
5. **`provision apply tasks/ci.yml` is green before you call it done.** Not
   "compiles", not "tests pass locally".
