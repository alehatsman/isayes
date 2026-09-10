#!/usr/bin/env python3
"""Report what a captured Claude Code session does to the screen.

Driven by scripts/measure.sh. Answers the questions in docs/measurements.md
against a raw PTY capture taken by scripts/capture.exp.

Every check is a regex over bytes here rather than a shell pipeline, because
the shell version reported "none" for four things that were present: BSD grep
has no -P, and the `|| true` that lets a legitimate no-match through swallowed
the usage error too.
"""

import re
import sys

BOLD, DIM, WARN, OK, OFF = "\033[1m", "\033[2m", "\033[33m", "\033[32m", "\033[0m"

MARKS = [b"===A-BARE-WINCH===", b"===B-REAL-RESIZE===", b"===C-DONE==="]

# (question, [patterns], yes-verdict, no-verdict)
CHECKS = [
    (
        "Q1  alternate screen (ESC[?1049h/l, and the 1047/47 forms)",
        [rb"\x1b\[\?[0-9;]*(?:1049|1047|47)[0-9;]*[hl]"],
        "it DOES — the margin must be re-asserted per repaint. Check D7's flicker budget.",
        "it does not. The margin survives once set.",
    ),
    (
        "Q2  its own scroll region (ESC[...r)",
        [rb"\x1b\[[0-9;]*r"],
        "it DOES — ours is destroyed and must be re-applied. MarginWatch is what catches it.",
        "it does not. Nothing contests the margin.",
    ),
    (
        "Q3  cursor save/restore (ESC 7 / ESC 8)",
        [rb"\x1b[78]"],
        "it DOES — §7 shares that single slot with it. Check where before trusting the bar.",
        "it does not. §7's save/restore is unshared.",
    ),
    (
        "Q4  soft/hard reset (ESC[!p, ESC c)",
        [rb"\x1b\[!p", rb"\x1bc"],
        "it DOES — both clear margins; both are in §7's table.",
        "it does not.",
    ),
    (
        "Q6  key-encoding modes it turns on (kitty, modifyOtherKeys)",
        [rb"\x1b\[>[0-9;]*u", rb"\x1b\[>4;[12]m"],
        "it DOES — modified keys will not arrive as §9's table assumes. See D11.",
        "it does not. §9's raw byte matching holds.",
    ),
    (
        "Q7  terminal-to-application traffic (focus, paste, theme, queries)",
        [rb"\x1b\[\?(?:1004|2004|2031)h", rb"\x1b\[c", rb"\x1b\[>0q"],
        "it DOES — stdin carries more than keystrokes. See D11.",
        "it does not.",
    ),
]


def main(path: str) -> int:
    try:
        with open(path, "rb") as fh:
            data = fh.read()
    except OSError as exc:
        print(f"cannot read capture: {exc}", file=sys.stderr)
        return 1

    if not data:
        print(
            f"empty capture: {path}\n"
            "  capture.exp's `drain` is what makes bytes reach the log; a bare\n"
            "  `sleep` does not consume the child's output and logs nothing.",
            file=sys.stderr,
        )
        return 1

    print(f"\n{BOLD}{path}{OFF}  ({len(data)} bytes)\n")

    for question, patterns, yes, no in CHECKS:
        hits = sum(len(re.findall(p, data)) for p in patterns)
        print(question)
        if hits:
            print(f"  {WARN}{hits:<6}{OFF} {yes}")
        else:
            print(f"  {OK}{'none':<6}{OFF} {no}")
        print()

    print("Q5  does it repaint on a bare SIGWINCH?")
    pos = [data.find(m) for m in MARKS]
    if any(p < 0 for p in pos):
        print(f"  {DIM}no markers — this capture was not taken by capture.exp{OFF}")
    else:
        bare = pos[1] - (pos[0] + len(MARKS[0]))
        real = pos[2] - (pos[1] + len(MARKS[1]))
        print(f"    bare SIGWINCH  -> {bare:6} bytes")
        print(f"    real resize    -> {real:6} bytes")
        if bare < 32 <= real:
            print(f"    {WARN}No repaint on a bare SIGWINCH; a dimension change does.{OFF}")
            print("    force_redraw()'s width toggle is REQUIRED. §11's idle rescue stays.")
        elif bare >= 32:
            print(f"    {OK}It repaints on SIGWINCH alone.{OFF}")
            print("    force_redraw()'s width toggle and §11's idle rescue can both go.")
        else:
            print("    Inconclusive — neither probe produced output. Re-run.")

    print(f"\n{DIM}Distinct sequences, for anything the checks above do not name:{OFF}")
    seen: dict[bytes, int] = {}
    for m in re.finditer(rb"\x1b(?:\[[0-9;?<>!$\"' ]*[A-Za-z~]|[0-9A-Za-z])", data):
        seen[m.group()] = seen.get(m.group(), 0) + 1
    # Cursor positioning and colour are the bulk and say nothing; drop them.
    noise = re.compile(rb"\x1b\[[0-9;]*[GKHmABCD]$")
    interesting = {k: v for k, v in seen.items() if not noise.match(k)}
    for seq, n in sorted(interesting.items()):
        label = seq.replace(b"\x1b", b"<ESC>").decode("latin1")
        print(f"    {label:22} {n}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print("usage: analyse.py <capture-file>", file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))
