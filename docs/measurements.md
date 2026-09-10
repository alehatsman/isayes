# What Claude Code actually does to the screen

Measured 2026-09-10 · Claude Code 2.1.236 · macOS 25.6 · plan.md task 1.0

Spec §7, §9 and D7 were written from cry-aye's behaviour and from inference.
This is the observation. Re-run it with `./scripts/measure.sh` when Claude Code
updates; anything below can change without notice, and none of it is
documented by Anthropic.

Method: `scripts/capture.exp` starts the TUI on a real PTY, logs every byte it
writes, sends a bare `SIGWINCH`, then an actual dimension change, then quits.
No prompt is submitted, so it makes no API call. 3 330 bytes captured.

## The five questions task 1.0 asked

| # | Question | Answer |
|---|---|---|
| 1 | Alternate screen (`ESC[?1049h`)? | **No.** Not once. It renders in the normal buffer. |
| 2 | Its own `DECSTBM`? | **Yes — once, at startup.** |
| 3 | `DECSC`/`DECRC` (`ESC 7`/`ESC 8`)? | **Yes — one pair, at startup.** |
| 4 | Soft/hard reset (`ESC[!p`, `ESC c`)? | **No.** |
| 5 | Repaint on a bare `SIGWINCH`? | **No.** |

### Q2 and Q3 are the same eight bytes

The first thing Claude Code writes to the terminal, at offset 0:

```
ESC 7    ESC [ r    ESC 8
DECSC    DECSTBM    DECRC
```

It **resets the scroll region to full height on startup**, wrapped in a cursor
save/restore. Consequences, both load-bearing:

- **A margin set before the child starts is destroyed by the child.** It must
  be applied *after* startup, and `MarginWatch` is what notices — this is the
  one `ESC[…r` in the whole capture, and catching it is not theoretical.
- **§7's claim that "Ink does not use `DECSC`" was false.** It uses it exactly
  once, as a tight adjacent pair in the first eight bytes, before our bar has
  anything to draw. The practical collision risk is nil and the claim still
  had to be corrected, because it was stated as a fact.

### Q5 settles `force_redraw`, against hope

| Probe | Bytes the child emitted |
|---|---|
| bare `SIGWINCH`, size unchanged | **2** |
| actual dimension change | **1 590** — `ESC[H`, 30× `ESC[2K`, 30× `ESC[1B` |

A bare signal produces nothing; a real resize produces a full repaint.

**`force_redraw()`'s width toggle is required, and §11's idle rescue with it.**
plan.md called this the largest simplification available in the port. It is not
available. The ugliest thing in the spec stays, and it now stays on evidence
rather than on inheritance.

## What the capture found that nobody asked

Startup, in order, after the margin reset:

```
ESC[?25l  ESC[?2004h  ESC[?1004h  ESC[?2031h  ESC[>4;2m  ESC[>1u  ESC[>0q  ESC[c
```

Four of those change **what arrives on our stdin**, and §9 assumed none of it.

| Sequence | Meaning | What it does to §9 |
|---|---|---|
| `ESC[>4;2m` | XTMODKEYS, `modifyOtherKeys=2` | The terminal may encode `Ctrl+A` as `ESC[27;5;97~` instead of `0x01`. |
| `ESC[>1u` | Kitty keyboard, disambiguate | The terminal may encode `Ctrl+A` as `ESC[97;5u`. |
| `ESC[?1004h` | Focus reporting | The terminal sends `ESC[I` / `ESC[O` when the window gains or loses focus. |
| `ESC[?2004h` | Bracketed paste | A paste arrives wrapped in `ESC[200~` … `ESC[201~`. |
| `ESC[c`, `ESC[>0q` | Primary DA, XTVERSION | The child *asks the terminal questions*. The replies arrive on our stdin. |

Two defects follow, and both are in cry-aye too:

**The hotkeys can silently never fire.** §9 matches raw bytes — `0x01` for
`Ctrl+A`, `ESC[1;5A` for `Ctrl+Up`. The child asks the terminal to stop sending
those. On a terminal that honours either mode — kitty, foot, iTerm2, WezTerm,
recent xterm — `Ctrl+A` does not arrive as `0x01` and the toggle is dead. It
works in Terminal.app, which implements neither, which is presumably why nobody
noticed.

**Terminal replies get eaten, and cancel a countdown on the way.** §9 says any
key that is not `Enter` cancels the countdown and is swallowed. A DA reply, a
focus event, a paste marker and a theme notification are all bytes on stdin
that are not keystrokes. Focus events are the sharp one: **clicking away from
the terminal during a countdown cancels the approval**, and the child never
receives the focus event it asked for.

Neither is fixable by matching more byte patterns. See D11.

## Not measured

- A real permission dialog's bytes. The detector corpus already carries six
  captured dialogs and the capture here submits no prompt on purpose.
- Behaviour on Linux, or under tmux — tmux rewrites most of the sequences
  above, and is the case most likely to differ.
- What the child does when the terminal *answers* its queries differently.
