# isayes — working notes

A PTY wrapper for `claude` that answers its permission dialogs. Rust port of
cry-aye (Go, at `~/projects/cry-aye` — the source of truth for behaviour and
for the test corpus).

**State: spec and seams done, wrapper not implemented.** `src/main.rs` is the
CLI surface and a stub that exits 3.

## Read in this order

1. `docs/spec.md` — the contract. 15 sections, 13 numbered invariants. Code
   that disagrees with it is wrong, or it is. Fix one, do not split the
   difference.
2. `docs/architecture.md` — the module map and the signatures between phases.
   Fixed before code so parallel work fits together.
3. `docs/plan.md` — phases, tasks, done-when.
4. `docs/decisions.md` — D1–D10, settled. To overturn one, say so first.

## The gate

```
provision apply tasks/ci-fast.yml   # pre-commit
provision apply tasks/ci.yml        # pre-push — this is "done"
```

Everyday commands are cargo aliases from `.cargo/config.toml`:

```
cargo lint      cargo t      cargo doctest      cargo docs
```

**`cargo` is not on PATH on this machine.** rustup is Homebrew's and
`~/.cargo/bin` has no shims. Prefix every invocation:

```
PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
```

`rustup default stable` would fix it permanently. That is the owner's call, not
ours.

## Non-negotiable

- **The engine gets no I/O and no clock** (D8). `Instant::now()` belongs in
  `events.rs` and nowhere else — `grep -rn 'Instant::now' src/` should show one
  file. This is what keeps the test suite deterministic and sub-second.
- **`unwrap`, `panic`, `todo!()` fail the gate** in non-test code. A stub that
  compiles green is worse than a red build.
- **Do not hand-edit `clippy.toml`, `rustfmt.toml`, `deny.toml`,
  `.cargo/config.toml`.** They are copies written by `rq/sync-config` from
  rust-quality (D6). Fix upstream.
- **Do not "fix" the code-block hazard in the detector** (§6). Quoted dialogs
  scoring like real ones is pinned by three fixtures on purpose.
- **The lint block in `Cargo.toml` is pasted, not authored.** `rq/lints-check`
  reports drift against rust-quality's `lints.toml`.

## Test data

`tests/fixtures/detector.toml` — 27 cases ported from cry-aye's Go tests and
verified against §6 as written. Tests loop over it; new dialogs go in as
entries, not as new test functions (D9).
