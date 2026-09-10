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

- Windows. POSIX PTY only (§13).
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
| `--delay N` | `0` | Seconds before an answer. Integer, `0..=60`. Out of range is a usage error, exit 1. |
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

Startup order, each step failing the process (§11) rather than degrading:

1. Size the PTY from the real terminal: `rows = height - 1` (floor 1),
   `cols = width`. The reserved row is §7.
2. Spawn `claude` with the pass-through args on the PTY.
3. Put stdin in raw mode.
4. Clear the screen (`ESC[2J ESC[H`), draw the status line.
5. Start the producers, enter the loop.

Teardown is one idempotent `cleanup`: restore the termios state, close the PTY,
kill the child, flush the debug log. It runs on every exit path including
panic.

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
| `(y/n)` at end of a line | 3 |
| `Permission rule` | 3 |
| `Esc to cancel` | 2 |
| `Tab to amend` | 2 |

**Threshold: `score >= 3`.** One weak indicator is never enough; the
yes/no button pair alone is. The threshold is the whole false-positive
defence — prose and code blocks that merely *mention* a yes/no reach 2 at
worst (§12 I6).

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
shared with the child. Ink does not use it, so today there is no conflict; a
child that did would need the cursor tracked instead, which means parsing, and
that is D7's rejected branch.

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
thing in this spec. It stays until Claude Code offers something better, and it
is a poke and not a corruption only because the scroll region holds the
resulting reflow inside the child's rows.

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
retries — the loop must not spin on a dead child (§12 I8).

## 9. Keys

Read from stdin before anything is forwarded. Consumed keys are never
forwarded to Claude.

| Key | Bytes | When | Effect |
|---|---|---|---|
| `Ctrl+A` | `0x01` | always | Toggle `auto_approve`; cancels any countdown. Enabling re-scans the buffer and starts a countdown if a dialog is sitting there. |
| `Ctrl+↑` | `ESC[1;5A` | no countdown | `delay + 1`, capped at 60. |
| `Ctrl+↓` | `ESC[1;5B` | no countdown | `delay - 1`, floored at 0. |
| `Enter` | `\r` \| `\n` | countdown | Answer now. |
| any other | | countdown | Cancel the countdown. The keystroke is swallowed. |
| any | | otherwise | Forwarded to the PTY verbatim. |

An empty read is ignored. A cancel leaves the buffer intact, so the same dialog
is re-detected on the next output or tick (§12 I9) — cancel is "not yet", not
"never".

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
| 1 | bad `--delay`; `claude` not on PATH or PTY spawn failed; stdin is not a TTY; an unrecoverable I/O error |
| 2 | panic — after `cleanup` |
| 128+signo | `SIGINT`, `SIGTERM`, `SIGHUP` — after `cleanup` |
| 3 | *temporary:* the wrapper is not implemented yet. Removed when §4 lands. |

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
10. **Rapid dialogs each get exactly one answer**, in order, with no deadlock.
11. **The status rows are unreachable by the child.** No volume of output, no
    resize, no alternate-screen toggle and no reset sequence leaves the bar
    scrolled, smeared or overwritten. Re-assert, do not hope.
12. **The terminal is handed back clean.** Full-height scroll region, cleared
    status rows, restored termios — on every exit path, including panic and
    signal.

## 14. Test contract

- **Unit** — `strip_ansi`, `is_prompt`, `needs_yes` against captured real
  dialogs, a false-positive corpus (prose, code blocks, transcripts), and a
  full-buffer sample with real escape sequences.
- **Loop** — a harness that drives the same event loop from an in-memory PTY
  pair with synthetic output. Never spawns `claude`. Covers every invariant in
  §13.
- **CLI** — `--delay` bounds, `--help`, `--` pass-through, exit codes.
- **Terminal** — the margin logic is a pure function over a chunk of child
  output (`needs_remargin(chunk) -> bool`) and a pure sequence builder
  (`margin(height) -> Vec<u8>`), so I11 is unit-testable without a terminal:
  feed `ESC[?1049h`, `ESC[?1049l`, `ESC[1;40r`, `ESC[!p`, `ESC c`, a split
  escape across two chunks, and a plain paragraph, and assert the verdict.
  Teardown (I12) is asserted on the byte stream a `Drop` produces.

Every invariant in §13 names at least one test.

## 15. Debug log

`ISAYES_DEBUG=1` appends to `~/.isayes-debug.log`: detection scores with the
indicators that matched, countdown transitions, answers, watchdog decisions,
and PTY write errors. Unset, nothing is opened and the log calls cost nothing.

The name is the port's, not cry-aye's — the two can be installed side by side
and must not share a file (D3).
