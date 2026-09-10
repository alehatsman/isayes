# isayes

A PTY wrapper for Claude Code that answers its permission dialogs.

`claude` runs inside a PTY. Every byte it writes goes straight to your
terminal, untouched. In parallel the wrapper scores a rolling copy of that
output against the shape of a permission dialog, and when the score crosses the
threshold it sends `\r` — after a countdown you can cancel, adjust, or turn off
entirely.

Rust port of [cry-aye](https://github.com/alehatsman/cry-aye).

## Install

```
cargo build --release                      # once, on a fresh clone
provision apply tasks/install.yml          # ~/.local/bin/isayes
provision apply tasks/install.yml --prop dest=~/bin/isayes
```

The first line is needed only once: provision validates a whole plan before
running any of it, so the copy step's source is checked before the build step
inside the task can create it. After that, `install.yml` rebuilds and copies on
its own, and reports `ok` rather than `changed` when the binary on PATH already
matches.

Or skip provision entirely and copy `target/release/isayes` yourself.

## Use

```
isayes                          # drop-in for `claude`
isayes --delay 3                # 3s to look at it and hit a key
isayes -- 'refactor this'       # a prompt
isayes -- --help                # Claude's flags, not ours
```

| Key | Action |
|---|---|
| `Ctrl+A` | Toggle auto-approve |
| `Enter` | Approve now |
| any other key | Cancel this countdown |
| `Ctrl+↑` / `Ctrl+↓` | Delay ± 1s |

The bottom row of the terminal is the wrapper's: `auto-approve ON  4 approved
delay 0s`. It is held there by a scroll region, not by hoping — see
[D7](docs/decisions.md).

`Ctrl+A` works whether your terminal sends `0x01`, `ESC[27;5;97~` or
`ESC[97;5u`. Claude Code turns on two keyboard protocols at startup, and
matching only the raw byte leaves the toggle dead on kitty, foot, WezTerm and
iTerm2 — that is [D11](docs/decisions.md), and a live bug in cry-aye.

## When it stops working

It will, eventually, and **it will not tell you.** Detection is pinned to
Claude Code's dialog strings; Anthropic owns those and does not version them. A
change to one drops the score below the threshold and the tool simply stops
approving, with no error.

That is the one failure mode worth knowing the drill for:

```
ISAYES_DEBUG=1 isayes
tail -f ~/.isayes-debug.log
```

```
[    1.037] below    score=3 hits=[permission_rule]
[    1.038] DETECTED score=12 hits=[yes_no_buttons,esc_to_cancel,tab_to_amend,permission_rule]
[    1.038] ANSWER [13] (#1)
```

Scores *below* the threshold are logged too, with the indicators that still
matched — a dialog that suddenly scores 2 instead of 12 tells you exactly which
string moved. The fix is a fixture in `tests/fixtures/detector.toml` and a row
in §6's table, not a redesign.

Re-run `./scripts/measure.sh` after a Claude Code update: everything in
[docs/measurements.md](docs/measurements.md) is undocumented behaviour that can
change without notice, and §7, §9, D7 and D11 all cite it.

## Two things to know before relying on it

**It says yes to the folder-trust prompt.** On a first run in an unfamiliar
directory, "Is this a project you trust?" is the first thing it approves. By
design — it says yes or it is off — but it is worth knowing which yes comes
first.

**A quoted dialog counts.** If Claude prints a code block containing a complete
permission dialog, that scores like a real one and gets answered. Nothing in
the byte stream separates a dialog Claude is *showing* from one it is
*quoting*, and every heuristic that suppresses the quote eventually suppresses
a real one. `Ctrl+A` and a non-zero `--delay` are the mitigations.

## Docs

| | |
|---|---|
| [docs/spec.md](docs/spec.md) | the contract — 15 sections, 13 invariants |
| [docs/architecture.md](docs/architecture.md) | module map and the seams |
| [docs/decisions.md](docs/decisions.md) | D1–D12 |
| [docs/measurements.md](docs/measurements.md) | what Claude Code actually does to the screen |
| [docs/plan.md](docs/plan.md) | phases and what is left |

## Develop

The gate is [rust-quality](https://github.com/alehatsman/rust-quality), driven
by [provision](https://github.com/alehatsman/provision). Config in this repo —
`clippy.toml`, `rustfmt.toml`, `deny.toml`, `.cargo/config.toml` — is written
by `rq/sync-config`; change it upstream, not here.

```
provision apply tasks/tools.yml        # once: checkout + nextest, deny, machete
provision apply tasks/sync-config.yml  # once: config in, lint block printed
provision apply tasks/ci-fast.yml      # pre-commit
provision apply tasks/ci.yml           # pre-push
```

Everyday commands are cargo aliases, so they work with no provision and no
YAML:

```
cargo lint      cargo t      cargo doctest      cargo docs
```

## License

MIT
