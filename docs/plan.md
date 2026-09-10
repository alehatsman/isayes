# isayes — build plan

Status: phases 0–4 built and gated · phase 1's four abuses are the only open item · 2026-09-10

## How to use this

Read [spec.md](spec.md) first — it is the contract. Then
[architecture.md](architecture.md) for the seams, then your phase below. Take
[decisions.md](decisions.md) as settled: if you want to overturn one, say so
before writing code, not in a pull request.

One phase, one worktree, one branch — `~/worktrees/isayes/<branch>`, outside
the repo. Conventional commits. A task is done when its **done-when** holds
*and* `provision apply tasks/ci.yml` is green. Not before.

| Phase | Branch | Gate | State |
|---|---|---|---|
| 0 — scaffold, spec, seams | `main` | seven tasks validate; gate green; corpus verified | done |
| 1 — terminal | `feat/terminal`, `feat/run` | the four abuses below | built; abuses 1–3 not yet run by hand |
| 2 — detector | `feat/detector` | 30 corpus cases green | done |
| 3 — engine | `feat/engine` | I1–I11 asserted, no sleeps | done |
| 3b — input (D11) | `feat/input` | hotkeys in all three encodings; reports never cancel | done |
| 4 — wiring, cut-over | `feat/run` | a real day's work under it | wired and running; cut-over is the owner's |

**1 and 2 are parallel.** They share no file. 3 needs only `detector`'s
signature and can start against a stub. See architecture.md's diagram.

---

## Phase 1 — the terminal

`terminal.rs`, `events.rs`. Spec §4, §7. Decisions D5, D7, D8.

First, because it is the part that is **broken today** and the only part whose
spec is inference rather than observation.

### 1.0 — Measure, before writing anything — **done**

Run `./scripts/measure.sh`. Results are in [measurements.md](measurements.md);
§7, §9 and D7 now cite it. Summary: no alternate screen, no resets, one
startup `DECSTBM` reset that destroys our margin, one `DECSC` pair, and **no
repaint on a bare `SIGWINCH`** — so `force_redraw` and §11's idle rescue both
stay. The capture also found two defects §9 did not know about (D11).

Re-run it when Claude Code updates. None of this is documented or stable.

**Done when:** every row below has an answer recorded in §7 or D7.

| Question | Why it matters |
|---|---|
| Does Claude Code drive the alternate screen (`ESC[?1049h`)? | If it does, the re-assert is per-repaint, not per-startup, and the flicker budget decides whether D7 is viable at all. |
| Does it emit its own `DECSTBM`? | It would fight the margin directly. Not in D7's trigger table as observed — as suspected. |
| Does it use `DECSC`/`DECRC`? | §7 shares that one save slot with the child. Ink is believed not to. If it does, the bar corrupts the child's cursor. |
| Does it repaint on `SIGWINCH` alone? | If yes, `force_redraw()`'s width toggle **and** §11's idle-rescue branch both delete themselves. That is the largest simplification available in this port. |

**Done when:** each row has an answer and a one-line note in §7 or D7 saying
what was seen, with the command that showed it.

### 1.1 — PTY and raw mode

Spawn `claude` on a PTY at `rows = height - STATUS_ROWS`, `cols = width`. Raw
mode on stdin. `Drop` restores termios.

**Done when:** `isayes` runs `claude` and it is usable — typing works, output
appears, `Ctrl+C` reaches the child. No bar yet.

### 1.2 — The margin — **done**

`margin(height)`, `RESET_MARGIN`, and `MarginWatch` — the sequences and the
scanner that says when the region has been destroyed. All pure, so this half of
I12 needed no terminal and no measurement: the trigger list is fixed VT
semantics regardless of what 1.0 finds about Claude.

15 tests. Every trigger in §7's table, every split-point including byte-at-a-
time, and the false positives that would cost a repaint on every colour change
— `?1000h`, `?2004h`, bare `ESC[0p`, OSC titles, cursor moves, SGR.

`needs_remargin` became `MarginWatch::feed`; it cannot be a free function
(architecture.md says why). Applying it at startup and on each trigger is
1.1/1.4's job.

**Done when:** `margin` and the scanner are asserted, without a terminal,
against every trigger in §7's table, the multi-parameter and legacy
alternate-screen forms, an escape split at every byte, and the near-misses that
must not fire — `?1000h`, `?2004h`, `ESC[0p`, `ESC[?2r`, OSC bodies.

### 1.3 — The bar

`draw_status`, on stderr, `ESC 7`/`ESC 8`, outside the region. Colours and
priority from §10.

**Done when:** the bar renders and every state in §10's two tables is
reachable by hand.

### 1.4 — Resize and teardown

`SIGWINCH` → size, PTY, margin, bar, in that order. `Drop` → `ESC[r`, clear the
rows, restore termios. On panic and on signal too.

**Done when:** the abuses below pass.

### 1.5 — `force_redraw`

Only if 1.0 says it is still needed.

### Phase 1 gate — the four abuses

Manual, in a real terminal, each one a way the bar dies today. Record the
result in the commit message.

1. **Volume.** `isayes -- 'print the numbers 1 to 10000'`, or run
   `yes | head -20000` inside the session. The bar does not scroll away, smear,
   or leave a copy of itself in the scrollback.
2. **Resize.** Drag the window narrower, then wider, then across a monitor
   boundary. Margin re-applied, child reflows, bar redrawn at the new width.
3. **Reset.** From inside the session, `printf '\033[?1049h'; sleep 1; printf
   '\033[?1049l'` and `printf '\033[!p'`. The margin comes back both times.
4. **Exit.** `Ctrl+C`; a forced panic; `kill -9` the child from another shell.
   Each leaves a full-height scroll region, no leftover bar, and a shell you
   can type in. `tput sgr0; tput cup 0 0` should not be needed to recover.

---

## Phase 2 — the detector — **done**

`detector.rs`. Spec §6. Decisions D9, D10. Built on `feat/detector`.

Pure functions, no state, no I/O, no clock. 27 corpus cases plus four written
directly: volume before a dialog, a dialog scrolled past the 50-line tail, hits
in table order, and a load guard that fails if the corpus ever comes back empty
rather than passing vacuously.

15 tests, 0.00 s, gate green. What landed differs from the sketch below in one
way: the crate gained a library target (D10), because a bin-only crate makes
every not-yet-wired module `dead_code` and there is no attribute that is
correct under both `cargo build` and `cargo lint --all-targets`.

### 2.1 — `strip_ansi`, `is_prompt`, `needs_yes`

Transcribe §6 exactly. Read §6's *two regex traps* before writing the regexes;
both are one flag away and both widen detection silently.

### 2.2 — The corpus runner

One test that loads the TOML and runs all five case kinds. A failure names the
case and prints the score and the indicator hits — a bare `assertion failed` on
case 14 of 27 is a waste of the corpus.

### 2.3 — `hits`, for the log

`Detection.hits` carries the indicator names §15 logs. It is the mitigation for
the liability below; it is not optional and not "later".

**Done when:** 27/27 green, and `is_prompt` on the largest corpus case runs in
well under the 200 ms tick with room to spare. It runs on every read.

---

## Phase 3 — the engine — **done**

`engine.rs`, `events.rs`. Spec §5, §8–§11. Decision D8. Built on `feat/engine`.

24 tests, 0.01 s, gate green — I1–I11 plus the key table, the two status texts,
the watchdog's rescue and its cooldown, and the exit codes. Nothing sleeps.

Two things came out of writing it:

- **`Action::Forward`** was missing from architecture.md. A forwarded keystroke
  and an answer are both PTY writes, but only one counts, and §9's "consumed
  keys are never forwarded" is unassertable without the distinction.
- **`last_rescue` is an `Option`.** Seeding it with the start time claims a
  rescue that never happened and holds the first real one off for the 3 s
  cooldown instead of the 2 s idle threshold. cry-aye gets this right by
  accident — Go's zero `time.Time` is far enough in the past to always pass.

`handle(&mut self, event: Event) -> Vec<Action>`. No I/O, no clock, no threads.
Tests construct an engine, feed events with invented instants, assert on the
returned actions. Nothing sleeps.

### 3.1 — Buffer and watermark

§5 and §8. The truncate-to-watermark, not clear.

### 3.2 — Countdown

Start, fire on the tick that reaches the deadline, `delay == 0` answers in the
same turn.

### 3.3 — Keys

§9's table, including the ones that are swallowed rather than forwarded.

### 3.4 — Watchdog

§11. Missed-dialog and idle-rescue, exclusive, with the 2 s and 3 s thresholds
driven by invented instants.

### 3.5 — The invariant suite

One test per invariant, named for it. The mapping is the deliverable:

| Inv | Asserts |
|---|---|
| I1 | the engine returns no action that touches output; `pass_through` is `main`'s |
| I2 | one dialog, one `Answer`, then nothing however many ticks follow |
| I3 | `auto_approve` off ⇒ no `Answer`, ever |
| I4 | a dialog split across two `Output` events answers once |
| I5 | 5 000 bytes of noise before a dialog still answers |
| I6 | the six `not_dialog` cases produce no `Answer` |
| I7 | the `Answer` payload is `yes\r`, in one action, in that order |
| I8 | `AnswerFailed` yields a flash and no retry — the next event is served |
| I9 | cancel, then the same buffer re-detects |
| I10 | dialog, answer, dialog ⇒ two `Answer`s |
| I11 | eight dialogs 20 ms apart ⇒ ≥1 `Answer`, loop still serving. Coalescing is allowed — see the invariant before asserting eight |

**Done when:** all eleven green and `cargo t` for the whole suite is under a
second. If it takes longer than that, a clock got read somewhere (D8).

---

## Phase 3b — the input parser (D11)

`input.rs`. Spec §9, decision D11. **Done.**

New scope, created by the 1.0 measurement rather than planned. 21 tests:
`Ctrl+A` in all three encodings, focus events and query replies as reports,
paste markers around a paste, sequences split at every byte, the lone-`Esc`
hold-and-flush, and the bounded recovery from an unterminated string sequence.

### 3b.1 — Wire it into the engine — **done**

`Engine` owns the parser; `on_input` feeds it and acts on the classification,
and the tick calls `flush` so a lone `Esc` resolves a read late.

Two things fell out of wiring it:

- **`Enter` is not a `Hotkey`.** It belongs to the child except while a
  countdown is running, and that is engine state the parser has no business
  knowing. It stays a `Unit::Key`; `input::is_enter` is how the engine asks.
- **Adjacent `Forward`s are coalesced.** The parser yields one unit per
  keystroke, so without merging, a 10 KiB paste would be 10 240 writes to the
  PTY. The bytes the child sees are identical; the syscall count is not.

## Phase 4 — wiring and cut-over

`main.rs`. Delete the `EX_NOT_IMPLEMENTED` stub and its exit code from §12.

Then use it instead of `claude` for a day. That is the only test that finds
what the other three phases agreed to be wrong about. Archive cry-aye when it
holds.

---

## Known liability

§6's table is pinned to Claude Code's current dialog strings — `1. Yes`,
`Enter to approve`, `Permission rule`. Anthropic owns those and does not
version them. A UI change silently drops the score below 3 and the tool stops
working with **no error at all**, which is the worst failure mode in this
design.

The mitigation is not a cleverer detector. It is `ISAYES_DEBUG=1` (§15), which
logs every score above zero with the indicators that matched — including the
ones that did **not** cross, because a dialog that suddenly scores 2 instead of
5 is the symptom. "It stopped approving" is one `tail` away from "the `Esc to
cancel` string moved", and the fix is a fixture plus a table row.

Built. A real run looks like this, and the first line is D12's fix working:

```
[    1.037] below    score=3 hits=[permission_rule]
[    1.038] DETECTED score=12 hits=[yes_no_buttons,esc_to_cancel,tab_to_amend,permission_rule]
[    1.038] ANSWER [13] (#1)
```

## Not in any phase

A rule engine, a config file, an approval log, Windows, a terminal emulator.
§2 and D7 say why.
