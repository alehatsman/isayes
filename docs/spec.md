# isayes — specification

Status: v0.1 · 2026-09-10 · owner: aleh

This is the contract. Code that disagrees with it is wrong, or this file is.
Fix one.

A Rust port of [cry-aye](https://github.com/alehatsman/cry-aye) (Go, ~800
production LOC). Behaviour below is the port's target, stated as a contract
rather than as a description of the Go code. Where the port deviates, see
[decisions.md](decisions.md).

## 1. Goal

Run `claude` inside a PTY, pass every byte through untouched, and answer its
permission dialogs on a timer the operator controls — with one key to turn the
answering off.

## 2. Scope

**In**

- Spawn `claude` on a POSIX PTY, proxy stdin and PTY output transparently.
- Detect Claude Code permission dialogs by scoring the recent output.
- Answer a detected dialog with `\r`, or `yes\r` when the dialog wants a word.
- A countdown before each answer, adjustable live, cancellable by any key.
- A one-line status bar on the terminal's last row.
- A watchdog for dialogs that arrive while the wrapper is not listening.

**Out**

- Windows. POSIX PTY only (D4).
- A rule engine — "approve `Bash`, refuse `Write`". It says yes or it is off.
- A config file, a profile, a persisted approval log.
- Rewriting or delaying Claude's output (§13 I1). It is read — for detection
  and for the scroll-region re-assert — and never altered.
- Emulating a terminal. The wrapper composites nothing (D7).
- Driving anything but `claude`.

## 3. CLI

```
isayes [OPTIONS] [--] [CLAUDE_ARGS...]
```

| Flag | Default | Rule |
|---|---|---|
| `--delay N` | `0` | Seconds before an answer. Integer, `0..=60`. Out of range is a usage error, exit 2 (§12). |
| `--help` | | Usage, options, examples, key table. Exit 0. |
| `--version` | | Crate version. Exit 0. |

Everything after `--` is passed to `claude` verbatim, including flags
(`isayes -- --help` shows Claude's help, not ours). With no `--` and no
recognised flag, a bare prompt string is passed through the same way.

## 4. Runtime shape

One event loop over one channel. Four producers, no shared mutable state
outside the loop:

| Producer | Emits |
|---|---|
| PTY reader thread | `Output(Vec<u8>)` — 4 KiB reads; `Eof` on close |
| stdin reader thread | `Input(Vec<u8>)` — 4 KiB reads |
| ticker thread | `Tick` every 200 ms |
| signal thread | `Winch`, `Terminate(signo)` |

Startup order, each step failing the process (§12) rather than degrading:

1. Size the PTY from the real terminal: `rows = height - STATUS_ROWS`
   (floor 1), `cols = width`. The reserved rows are §7.
2. Spawn `claude` with the pass-through args on the PTY.
3. Put stdin in raw mode.
4. Clear the screen (`ESC[2J ESC[H`), apply the scroll region, draw the bar.
5. Start the producers, enter the loop.

The margin goes on **after** the child is spawned, and even that is not enough
on its own: the child's own first act is a full-height `DECSTBM` reset
(measured — §7), which arrives asynchronously and will destroy whatever we set.
`MarginWatch` catching that reset is not a defensive nicety, it is the
mechanism by which the margin exists at all.

Teardown is one idempotent `cleanup`: restore the termios state, close the PTY,
kill the child, flush the debug log. It runs on every exit path including
panic.

**Time is an input, never an ambient fact.** The loop does not call
`Instant::now()`. `Tick` carries the instant it fired, `Output` and `Input` are
stamped as the loop receives them, and every deadline comparison is against
that stamp. The producer threads own the real clock; the loop owns none of it.

This is one constraint and it buys the whole test strategy (D8): the harness in
§14 drives a 60-second countdown, a 2-second idle rescue and eight overlapping
dialogs by handing the loop a made-up `Instant`, in microseconds, with no
sleeps and no flakes. cry-aye's equivalent suite is roughly a minute of
`time.Sleep` and races on a slow machine. Any function that reads the clock
behind the loop's back puts that back.

## 5. The buffer

Output is appended to a rolling byte buffer capped at **10 000 bytes**; the
oldest bytes are dropped past the cap. It is the only input to detection.

It is appended to *after* the bytes have already been written to stdout —
detection never sits in the output path.

## 6. Detection

`is_prompt(buffer) -> (bool, score)`.

Strip ANSI first, then keep only the **last 50 lines** — a dialog is always the
most recent output, and a tail bounds the cost of scanning on every read.

**Strip**, in this order:

1. Cursor-movement CSI `\x1b\[[\d;]*[ABCDEFGHJKfsu]` → one space. The
   substitution is a space, not nothing: a cursor jump is a visual gap, and
   collapsing it would join two unrelated words into a false match.
2. Remaining escapes `\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|\][^\x07]*\x07)` →
   removed.
3. Control characters `[\x00-\x08\x0B-\x0C\x0E-\x1F]` → removed. `\t` (0x09)
   and `\n` (0x0A) survive; line structure is load-bearing.
4. `\r` → space.
5. Runs of spaces → one space.

**Score** the stripped tail. Additive, no early exit:

| Indicator | + |
|---|---|
| `1. Yes` \| `1) Yes` \| `• Yes` **and** `[23][.)]\s*No` \| `• No` | 5 |
| `Enter to approve` \| `Enter to confirm` | 3 |
| `(y/n)` at the end of the tail, trailing whitespace allowed | 3 |
| `Permission rule` | 3 |
| `Esc to cancel` | 2 |
| `Tab to amend` | 2 |

**Threshold: `score >= 3`.** One weak indicator is never enough; the
yes/no button pair alone is. The threshold is the whole false-positive
defence — prose that merely *mentions* a yes/no reaches 2 at worst (§13 I6).

**Known hazard, not a bug.** A code block or a transcript that reproduces a
*complete* dialog — `1. Yes`, `2. No`, `Enter to approve` — scores like the
real thing and gets answered. cry-aye's `TestIsPrompt_CodeBlockSafety` asserts
exactly that: all three of its fenced-code cases detect. The name is a
misnomer; the test documents the hazard rather than defending against it.

Do not try to fix this in the detector. Nothing in the byte stream separates a
dialog Claude is *showing* from one Claude is *quoting* — backtick counting
fails the moment output is truncated or a fence is split across reads, and any
heuristic that suppresses a quoted dialog will eventually suppress a real one,
which is the far worse failure. `Ctrl+A` and a non-zero `--delay` are the
mitigations. A port that "improves" on this breaks three ported tests.

**Two regex traps, both silent.** Neither is a Rust/Go difference — the
defaults agree, and the danger is an implementer "fixing" them:

- `\(y/n\)\s*$` is **not** multi-line. `$` means end of the tail, not end of a
  line, in Go's RE2 and in Rust's `regex` alike. Adding `(?m)` makes a `(y/n)`
  anywhere in the scrollback score 3, and 3 is the threshold.
- `Enter.*yes` is case-insensitive but **not** dot-matches-newline. Adding
  `(?s)` makes `1. Yes … Enter to approve` answer with a literal `yes`, which
  a button dialog reads as a prompt edit. The corpus pins this case.

`needs_yes(buffer)` — case-insensitive `Type.*yes | Enter.*yes | \(y/n\)` on
the stripped buffer. It decides the answer's bytes, nothing else.

## 7. Terminal ownership

`STATUS_ROWS = 1`. The wrapper owns the bottom `STATUS_ROWS` rows of the real
terminal; Claude gets rows `1 ..= height - STATUS_ROWS`.

**Resizing the PTY is not enough, and believing it was is the port's biggest
inherited bug.** A `winsize` is *advisory*: it tells the child how much room it
thinks it has, and the child's bytes still go to the real terminal, which has
all `height` rows. The moment the child emits one newline past its last row the
real terminal scrolls — status row included. The bar smears, drifts up, or gets
half-overwritten, and the display never recovers.

The fix is a **scroll region**. Three mechanisms, all three required:

1. **PTY size** — `rows = height - STATUS_ROWS` (floor 1), `cols = width`. So
   the child lays out for the space it actually has.
2. **`DECSTBM`** — `ESC[1;<height - STATUS_ROWS>r` on the real terminal. Now
   scrolling is confined to that region by the terminal itself, and the status
   rows are physically unreachable by the child's output, whatever it emits.
   After setting it the cursor homes, so a redraw follows.
3. **Re-assert.** A margin is not permanent state a wrapper can set once:

   | Trigger | Why the margin is gone |
   |---|---|
   | `SIGWINCH` | margins are defined in terms of a size that just changed |
   | child writes `ESC[?1049h` / `l` | the alternate screen has its own margins |
   | child writes its own `ESC[…r` | it set a full-height region |
   | child writes `ESC[!p` (`DECSTR`) or `ESC c` | soft/hard reset clears margins |

   The output path scans for those sequences — the only inspection of the
   stream anywhere in this tool (§13 I1 permits reading, never rewriting) — and
   re-applies the margin after them. `SIGWINCH` re-applies in order: read the
   size, resize the PTY, set the margin, redraw the bar.

Routing:

- Claude's output → **stdout**, byte for byte.
- The status bar → **stderr**, wrapped in `ESC 7` / `ESC 8` so the cursor
  returns to wherever the child left it.
- Bar body: `ESC[<row>;1H ESC[K ESC(B ESC[<colour>m <text> ESC[0m`, drawn
  outside the scroll region, so it cannot scroll.

`ESC 7` / `ESC 8` (`DECSC`/`DECRC`) is a single-slot save per screen buffer,
shared with the child. **Measured 2026-09-10: the child does use it** — one
pair, in the first eight bytes, wrapping its own margin reset. That is before
the bar has anything to draw, so the collision risk is nil in practice, but the
slot is shared and the earlier claim that Ink never touches it was wrong. A
child that used it around a longer operation would need the cursor tracked
instead, which means parsing, and that is D7's rejected branch.

**The child destroys our margin on startup.** Its first act is
`ESC 7  ESC[r  ESC 8` — a full-height `DECSTBM` reset. A margin set before
spawning does not survive, so the startup order in §4 applies it *after* the
child is running, and `MarginWatch` catches the reset if the timing slips.
See [measurements.md](measurements.md).

**Teardown restores the terminal**: `ESC[r` (full-height region), clear the
status rows, restore termios. In that order, on every exit path — a leftover
margin leaves the user's shell rendering into a box.

**`force_redraw()`** — set the PTY to `cols - 1`, then after **50 ms** restore
`cols`. Rows stay at `height - STATUS_ROWS` throughout. Guard: no-op below 2×2.

Two reasons, both load-bearing:

- Ink skips a repaint when the dimensions have not changed, so a plain resize
  to the same size does nothing.
- The 50 ms gap keeps the kernel from coalescing the two `SIGWINCH`s. Without
  it the child can read the already-restored width, conclude nothing changed,
  and skip the repaint after all.

It is a poke at a child that will not repaint on request, and it is the ugliest
thing in this spec. **Measured 2026-09-10: it is also necessary.** A bare
`SIGWINCH` with the size unchanged produced 2 bytes from the child; an actual
dimension change produced 1 590 and a full repaint. There is no signal that
makes it redraw without a real size change, so the toggle stays, and §11's idle
rescue with it. It is a poke and not a corruption only because the scroll
region holds the resulting reflow inside the child's rows.

## 8. Answering

State: `auto_approve: bool` (starts **on**), `countdown: Option<Countdown>`,
`approvals: u32`.

**Start.** On new output, with `auto_approve` on and no countdown running, if
`is_prompt` holds: start a countdown ending `delay` seconds out and record the
**watermark** — the buffer's length at that instant. `delay == 0` answers in
the same turn rather than waiting a tick.

**Fire.** On the tick that reaches the deadline, or on `Enter` during the
countdown.

**Answer**, in order:

1. Clear the countdown, `approvals += 1`.
2. Decide `needs_yes` from the *whole* buffer.
3. Truncate the buffer to the bytes **after the watermark**.
4. Write `yes\r`, or `\r`, to the PTY. `yes` and the `\r` go in one write, in
   that order.
5. Flash `✓ Auto-approved (#N)`.
6. `force_redraw()`.

**The watermark is the whole design.** Everything before it is the dialog just
answered; keeping it would re-detect and re-answer the same dialog forever.
Everything after it arrived while the countdown was running, when detection was
switched off — it may be a second dialog, and dropping it would lose an answer.
Truncation, not a clear.

A failed PTY write flashes `✗ Failed to send approval` and returns. It never
retries — the loop must not spin on a dead child (§13 I8).

## 9. Keys

Read from stdin before anything is forwarded. Consumed keys are never
forwarded to Claude.

| Key | Bytes | When | Effect |
|---|---|---|---|
| `Ctrl+A` | `0x01` | always | Toggle `auto_approve`; cancels any countdown. Enabling re-scans the buffer and starts a countdown if a dialog is sitting there. |
| `Ctrl+↑` | `ESC[1;5A` | no countdown | `delay + 1`, capped at 60. |
| `Ctrl+↓` | `ESC[1;5B` | no countdown | `delay - 1`, floored at 0. |
| `Enter` | `\r` \| `\n` | countdown | Answer now. |
| any other **keystroke** | | countdown | Cancel the countdown. The keystroke is swallowed. |
| any | | otherwise | Forwarded to the PTY verbatim. |

An empty read is ignored. A cancel leaves the buffer intact, so the same dialog
is re-detected on the next output or tick (§13 I9) — cancel is "not yet", not
"never".

### Stdin is not only keystrokes

Measured 2026-09-10 (see [measurements.md](measurements.md)): on startup the
child turns on focus reporting (`ESC[?1004h`), bracketed paste (`ESC[?2004h`)
and theme notifications (`ESC[?2031h`), and it *queries* the terminal with
Primary DA (`ESC[c`) and XTVERSION (`ESC[>0q`). Every one of those makes the
terminal send bytes to **us**, because we own the tty, and none of them is a
keypress:

| Arrives | When |
|---|---|
| `ESC[I` / `ESC[O` | the window gains or loses focus |
| `ESC[200~` … `ESC[201~` | around a paste |
| `ESC[?…c`, a DCS version string | the child's queries being answered |
| `ESC[?997;1n` | the OS theme changed |

Two rules follow, and both are corrections rather than additions:

1. **A terminal report is forwarded and does not cancel.** Treating it as "any
   other key" swallows a reply the child is blocking on and cancels a countdown
   for a reason the operator never caused. Clicking away from the window during
   a countdown must not cancel the approval.
2. **A hotkey is recognised in every encoding the child asked for.** The same
   startup enables `modifyOtherKeys=2` (`ESC[>4;2m`) and the Kitty keyboard
   protocol (`ESC[>1u`), so on a terminal that honours either, `Ctrl+A` arrives
   as `ESC[27;5;97~` or `ESC[97;5u` — not as `0x01`. Matching only the raw byte
   means the toggle silently does nothing on kitty, foot, WezTerm, iTerm2 and
   recent xterm. It works in Terminal.app, which implements neither, which is
   presumably why cry-aye never noticed.

Both are D11. Neither is solvable by adding byte patterns to the table above:
the wrapper has to know where an escape sequence *ends* before it can decide
what the bytes were.

## 10. Status line

Highest priority first:

| State | Text | Colour |
|---|---|---|
| flash active | the flash message | per flash |
| countdown | `⏱  Auto-approving in Ns... (Enter=now, any key=cancel, Ctrl+A=off)` | 33 |
| on | `auto-approve ON  N approved  delay Ns  [Ctrl+A=toggle, Ctrl+↑↓=delay]` | 2 |
| off | `auto-approve OFF  delay Ns  [Ctrl+A=toggle, Ctrl+↑↓=delay]` | 90 |

Countdown seconds are rounded **up**, floored at 0, so a 3 s delay reads
`3 → 2 → 1` and never `0` twice.

| Flash | Colour | Hold |
|---|---|---|
| `✓ Auto-approved (#N)` | 32 | 800 ms |
| `✗ Failed to send approval` | 31 | 1 s |
| `✓ Auto-approve ENABLED` | 32 | 800 ms |
| `✗ Auto-approve DISABLED` | 31 | 800 ms |
| `✗ Cancelled` | 90 | 500 ms |
| `⏱  Delay: Ns → Ms` | 36 | 800 ms |

The delay flash is emitted only when the value actually moved — at the cap or
the floor the key is silent.

## 11. The tick

Every 200 ms, in order: fire an expired countdown, run the watchdog, redraw the
status line.

The watchdog runs only with `auto_approve` on and no countdown, and does one of
two things:

- **Missed dialog** — the buffer is non-empty and `is_prompt` holds. A dialog
  that arrived while a countdown was running was never offered to detection;
  this is the only thing that finds it. Start a countdown.
- **Idle rescue** — no PTY output for **≥ 2 s** and no rescue in the last
  **3 s**: `force_redraw()`. A dialog Claude has already painted produces no
  further bytes, so nothing would ever re-enter §6 on its own. The redraw makes
  it re-flow through the output path.

The two are exclusive; the missed-dialog branch returns.

## 12. Exit

| Code | Cause |
|---|---|
| child's | `claude` exited; its status is propagated |
| 0 | `--help`, `--version` |
| 1 | `claude` not on PATH or PTY spawn failed; stdin is not a TTY; an unrecoverable I/O error |
| 2 | usage error — a bad `--delay`, an unknown flag |
| 3 | *temporary:* the wrapper is not implemented yet. Removed when §4 lands. |
| 101 | panic — after `cleanup` |
| 128+signo | `SIGINT`, `SIGTERM`, `SIGHUP` — after `cleanup` |

Two of those are deviations from cry-aye, both deliberate (D3): it exits 1 on a
bad `--delay` and 2 on a panic. 2 is clap's code for a usage error and 101 is
what a panicking Rust process returns on its own; overriding either would mean
writing code whose only purpose is to disagree with the ecosystem's default.

`SIGWINCH` is not an exit: re-read the size, resize the PTY, re-apply the
scroll region, redraw the bar — in that order (§7).

## 13. Invariants

The properties the tests exist to hold. Each one has cost a bug once.

1. **Output is never touched.** Not modified, not reordered, not delayed by
   detection. Detection reads a copy.
2. **One answer per dialog.** The watermark (§8) is what guarantees it.
3. **Off means off.** No answer is ever sent with `auto_approve` false.
4. **A split dialog still detects.** The buffer is cumulative; a dialog
   delivered in two reads scores the same as one.
5. **Volume does not blind it.** 10 KB of build output before a dialog does not
   push it out of the 50-line tail.
6. **Prose is not a dialog.** A code block or a transcript that mentions
   `1. Yes` without the rest stays under the threshold.
7. **`yes` precedes `\r`.** In one write, in that order.
8. **A dead PTY does not spin.** A write failure flashes and returns.
9. **Cancel is not permanent.** The same dialog is re-detected afterwards.
10. **Sequential dialogs each get their own answer.** One that arrives after
    the previous was answered is answered in its turn — the watermark narrows
    the buffer, it does not deafen the tool.
11. **Overlapping dialogs coalesce, and never deadlock.** Dialogs arriving
    faster than the countdown may yield fewer answers than dialogs. That is
    correct, not a defect: the redraw after each answer re-surfaces whatever
    is still pending, so nothing is lost, only merged. What is guaranteed is
    that at least one answer is sent and the loop keeps serving events.
12. **The status rows are unreachable by the child.** No volume of output, no
    resize, no alternate-screen toggle and no reset sequence leaves the bar
    scrolled, smeared or overwritten. Re-assert, do not hope.
13. **The terminal is handed back clean.** Full-height scroll region, cleared
    status rows, restored termios — on every exit path, including panic and
    signal.

## 14. Test contract

- **Detector** — a loop over `tests/fixtures/detector.toml` (D9): captured real
  dialogs with a minimum score, a false-positive corpus, the quoted-dialog
  hazard cases, `strip_ansi` pairs and `needs_yes` pairs. 27 cases, verified
  against this section as written.
- **Engine** — construct an `Engine`, feed it `Event`s stamped with invented
  instants, assert on the `Vec<Action>` returned. No PTY, no terminal, no
  threads, no sleeps, no `claude`. Every invariant I1–I11 is reachable this
  way, and the whole suite runs in under a second — if it does not, something
  read the clock (D8).
- **CLI** — `--delay` bounds, `--help`, `--` pass-through, exit codes.
- **Terminal** — the margin logic is a pure sequence builder
  (`margin(height) -> Vec<u8>`) and a pure scanner (`MarginWatch::feed`), so
  I12 is unit-testable without a terminal. The scanner cannot be the free
  function this section first specified: an escape splits across PTY reads at
  any byte, so the carry has to live somewhere. Feed it:
  feed `ESC[?1049h`, `ESC[?1049l`, `ESC[1;40r`, `ESC[!p`, `ESC c`, a split
  escape across two chunks, and a plain paragraph, and assert the verdict.
  Teardown (I13) is asserted on the byte stream a `Drop` produces.

Every invariant in §13 names at least one test.

## 15. Debug log

`ISAYES_DEBUG=1` appends to `~/.isayes-debug.log`: detection scores with the
indicators that matched, countdown transitions, answers, watchdog decisions,
and PTY write errors. Unset, nothing is opened and the log calls cost nothing.

The name is the port's, not cry-aye's — the two can be installed side by side
and must not share a file (D3).
