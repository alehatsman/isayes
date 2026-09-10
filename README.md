# isayes

A PTY wrapper for Claude Code that answers its permission dialogs.

`claude` runs inside a PTY. Every byte it writes goes straight to your
terminal, untouched. In parallel the wrapper scores a rolling copy of that
output against the shape of a permission dialog, and when the score crosses the
threshold it sends `\r` — after a countdown you can cancel, adjust, or turn off
entirely.

Rust port of [cry-aye](https://github.com/alehatsman/cry-aye).
**Status: spec and seams written, wrapper not implemented.**

| | |
|---|---|
| [docs/spec.md](docs/spec.md) | the contract — 15 sections, 13 invariants |
| [docs/architecture.md](docs/architecture.md) | module map and the signatures between phases |
| [docs/plan.md](docs/plan.md) | phases, tasks, done-when |
| [docs/decisions.md](docs/decisions.md) | D1–D9 |

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
[D7](docs/decisions.md#d7--a-scroll-region-for-the-status-bar-not-a-compositor).

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
