#!/usr/bin/env bash
# measure.sh — what Claude Code actually does to the screen.
#
# Spec §7, §9 and D7 rest on beliefs about the child's behaviour. This captures
# a real session on a PTY and reports what is in it. Results as of 2026-09-10
# are in docs/measurements.md; re-run this when Claude Code updates.
#
#   ./scripts/measure.sh              capture a session, then analyse it
#   ./scripts/measure.sh <file>       analyse a capture taken earlier
#
# The capture is raw PTY bytes — everything on screen during the session. Read
# it before pasting it anywhere. It submits no prompt, so it costs no tokens.
#
# The analysis is Python, not grep, and that is deliberate. The first version
# used `grep -aoP`, which BSD grep does not support; paired with the `|| true`
# needed to survive a legitimate no-match, every question came back "none" —
# including four that were demonstrably present in the same capture. A checker
# that reports clean when it is broken is worse than no checker.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

capture() {
    local out="$1"
    command -v claude >/dev/null || { echo "claude is not on PATH" >&2; exit 1; }
    command -v expect >/dev/null || { echo "expect is not installed" >&2; exit 1; }
    # Claude Code sets CLAUDECODE and friends; a child started from inside a
    # session should look like a plain terminal, which is what we measure.
    env -u CLAUDECODE -u CLAUDE_CODE_ENTRYPOINT -u CLAUDE_CODE_SESSION_ID \
        expect "$here/capture.exp" "$out" claude >/dev/null 2>&1 || true
}

main() {
    local capfile
    if [ $# -ge 1 ]; then
        capfile="$1"
    else
        capfile="${TMPDIR:-/tmp}/isayes-capture.raw"
        capture "$capfile"
    fi
    python3 "$here/analyse.py" "$capfile"
}

main "$@"
