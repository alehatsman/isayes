# isayes — decisions

Short records. Each has a decision, the reason, and what would overturn it.

## D1 — Rust, one crate, one binary

**Decision.** Rust 2024, a single binary crate, no workspace members. The
empty `[workspace]` table exists only so rust-quality's `[workspace.lints]`
block has somewhere to hang.

**Why.** The fleet's next stack. The job — a PTY, two reader threads, a
termios restore that must survive a panic — is exactly where Rust's `Drop` and
its typed errors are worth the trouble. Go's version leaked a raw terminal on
some exit paths and needed a `recover()` to paper over it.

**Overturned by.** Nothing foreseeable.

**Amended 2026-09-10 by D10.** One crate, but two targets — a library holding
the modules and a thin binary. The reason is the gate, not taste; see D10.

## D2 — Threads and one channel, not async

**Decision.** Three producer threads (PTY, stdin, ticker) plus a signal
handler feed one `std::sync::mpsc` channel; a single-threaded loop owns all
mutable state. No tokio.

**Why.** Go's `select` over four channels maps onto `Recv` on one channel with
an `Event` enum, and that is the whole translation. The workload is four file
descriptors and a 200 ms tick — an async runtime would buy nothing and cost a
dependency tree, and `AsyncRead` over a PTY is the one place tokio is
genuinely awkward on macOS. Single-owner state also removes every lock the Go
version relied on being lucky about: it shared `buffer` across goroutines with
no mutex.

**Overturned by.** A need to drive more than one child.

## D3 — A port, not a fork

**Decision.** cry-aye stays where it is. isayes is a fresh repo, a new binary
name, and a new debug-log path (`~/.isayes-debug.log`, `ISAYES_DEBUG=1`).

**Why.** Both can be installed at once during the port, and a shared debug log
would interleave two processes' lines into nonsense. Everything else is
behaviour-identical on purpose: the Go tests are the port's acceptance
criteria, so a deliberate difference has to be written down here or it is a
regression.

**Overturned by.** cry-aye being archived, at which point the name is free.

## D4 — POSIX only

**Decision.** No Windows. No ConPTY.

**Why.** The tool's whole surface is `openpty`, `TIOCSWINSZ`, termios raw mode
and `SIGWINCH`. ConPTY has an analogue for each and a different failure mode
for all of them, and nobody in this fleet runs `claude` from a Windows console.

**Overturned by.** Someone actually asking.

## D5 — Crate picks

**Decision.** Beyond `clap`, the port takes `portable-pty` for the PTY,
`crossterm` for raw mode and terminal size, `signal-hook` for
`SIGWINCH`/`SIGTERM`, and `regex` for §6.

**Why.** [STACK.md](../../rust-quality/docs/STACK.md) has no PTY or terminal
row, so these four are a deviation to record rather than a default to cite.
`portable-pty` (wezterm's) is the one PTY crate with a maintained release and
a real user; `crossterm` is what ratatui stands on; `signal-hook` is the only
sound way to get a signal into a channel in safe Rust. `regex` is a STACK
default and its linear-time guarantee matters here — the scanner runs on every
read.

The escape sequences in spec §7 are written by hand, not through crossterm's
API: `DECSTBM` has no crossterm command, and a status bar that half-uses a
library and half-emits raw bytes is harder to read than one that emits raw
bytes throughout.

The dependency list stays empty of all four until the code that uses them
lands: an unused dependency fails `rq/ci` on `cargo-machete`, which is the
correct outcome.

**Overturned by.** `portable-pty` going unmaintained — the fallback is
`rustix::pty` plus roughly forty lines of our own `fork`/`exec`, which is the
part of this decision worth avoiding, not the crate.

## D6 — Setup is provision + rust-quality, like every other repo here

**Decision.** `tasks/*.yml` for provision; the gate is
[rust-quality](https://github.com/alehatsman/rust-quality) pinned in
`tasks/tools.yml` and nowhere else. `clippy.toml`, `rustfmt.toml`, `deny.toml`
and `.cargo/config.toml` are copies written by `rq/sync-config` — edit them
upstream, not here. The lint block in `Cargo.toml` is pasted because cargo has
no include mechanism for manifests; `rq/lints-check` reports its drift.

**Why.** The point of a fleet gate is that a second repo costs seven small
YAML files and no thinking. This repo is the second one.

**Overturned by.** Nothing. Divergence here is a bug in rust-quality to be
fixed upstream.

## D7 — A scroll region for the status bar, not a compositor

**Decision.** Reserve the bottom `STATUS_ROWS` rows with `DECSTBM`
(`ESC[1;<h-STATUS_ROWS>r`) on the real terminal, size the PTY to match, and
re-assert the margin whenever the child does something that clears it
(spec §7). The child's bytes still go to stdout untouched.

**Why.** cry-aye reserves the row by sizing the PTY one row short and asserts
that the child "can never render there". That is false — a `winsize` is
advisory, the child's output reaches the real terminal, and one scroll past the
bottom takes the status bar with it. This is the observed breakage. `DECSTBM`
is the mechanism that actually makes the reservation physical, it is one escape
sequence, it is in every terminal worth supporting, and it keeps the output
path byte-transparent.

Rows, not pixels: a terminal is a cell grid, so "20px" is `STATUS_ROWS`, and
one row holds `auto-approve OFF delay 0s` with room left. The constant exists
so a second row is a one-line change, not a rewrite.

**Rejected: full compositing.** Parse the child's output with `vte`/`termwiz`
into a cell grid and render that grid into the top region ourselves — what tmux
does. It is the only approach that is unconditionally correct, and it costs a
VT emulator: a screen model, scrollback, OSC, mouse reporting, wide characters,
sixel, and a permanent bug queue. It also breaks the one invariant this tool
is built on — output passes through untouched. Not worth it to own one row.

**Overturned by.** A margin that cannot be kept. If some Claude Code release
starts driving the alternate screen in a way that fights the re-assert — a
visible flicker on every repaint, a margin lost faster than it is restored —
then the reservation is not obtainable by cooperation and compositing is the
only remaining answer. That is a rewrite of §7 and nothing else; the detector,
the loop and the watermark do not care.

## D8 — The loop never reads the clock

**Decision.** Time enters the event loop as data. `Tick(Instant)` carries the
instant it fired; `Output` and `Input` are stamped on receipt; deadlines are
compared against that stamp. `Instant::now()` appears in the producer threads
and nowhere else (spec §4).

**Why.** cry-aye's test suite is built on `time.Sleep`. Twenty-three tests,
sleeps from 20 ms to 3 s, a rapid-fire case that can run fifteen seconds, and a
write-failure case that sleeps three seconds and asserts nothing at all — it is
a hang detector wearing a test's clothes. It is slow, it races on a loaded
machine, and it can only assert coarse outcomes, because anything finer is a
timing gamble.

Stamped events remove all of it. A 60-second countdown, the 2-second idle
threshold and the 3-second rescue cooldown are all exercised by handing the
loop an `Instant` that is 60 seconds later. The suite runs in microseconds,
asserts exact transitions instead of "at least one approval eventually", and
cannot flake.

The cost is one field on an enum variant and the discipline not to call `now()`
in a helper. That is one careless line to reintroduce, so it is written down
here rather than left as a style someone might notice.

**Overturned by.** Nothing. A future feature that genuinely needs wall-clock
inside the loop gets another stamped event.

## D9 — The detector corpus is data, not Rust

**Decision.** cry-aye's detector cases live in `tests/fixtures/detector.toml`
— inputs, expected verdict, expected minimum score — and the Rust tests are a
loop over that file.

**Why.** The corpus is the actual contract of §6, it came from a Go test file
nobody will keep reading, and it is the one artifact several agents need at
once. As data it is reviewable in a diff, extendable by anyone who captures a
new dialog, and impossible to half-port. As inline Rust it would be copied,
reworded and quietly diverged.

Captured dialogs go in as new entries. That is the maintenance path for the
liability in plan.md: when Claude Code changes a string, the fix is a fixture
plus a row in §6's table, not a redesign.

**Overturned by.** Nothing. If the corpus grows past a few hundred cases it
splits into files per category, still data.

## D10 — A library target, with the binary as a thin shell

**Decision.** `src/lib.rs` holds the modules; `src/main.rs` is the CLI and the
wiring. Module surfaces are `pub`, not `pub(crate)`.

**Why.** Forced by the gate, and worth having anyway. In a bin-only crate every
module the binary has not wired up yet is `dead_code` — an error under
`-D warnings` — so a phase that lands before the phase that consumes it cannot
be green. The obvious patch, `#[expect(dead_code)]`, makes it worse:
`cargo lint` runs `--all-targets`, the tests *do* use the code, the expectation
goes unfulfilled, and `unfulfilled_lint_expectations` fires instead. There is
no attribute that is correct in both targets.

A lib target removes the question. Public items in a library are reachable by
definition, so each phase lands green on its own, in any order, which is what
the parallel plan depends on. It also turns `missing_docs` from a lint that
never fires into one that does — an upside, not a tax.

**Overturned by.** Nothing. The binary stays thin; logic that appears in
`main.rs` belongs in a module.
