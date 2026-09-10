# isayes — working notes

A PTY wrapper for `claude` that answers its permission dialogs. Rust port of
cry-aye (Go, at `~/projects/cry-aye` — the source of truth for behaviour and
for the test corpus).

**State: detector, engine and the margin scanner are done and tested. The
wrapper does not run yet** — `src/main.rs` is still the CLI surface and a stub
that exits 3. What is missing is phase 1's raw mode, PTY, bar and teardown, and
phase 4's wiring. Check `docs/plan.md` before writing anything: roughly 1 500
lines already exist under `src/`.

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

`cargo` was missing from PATH on this machine until 2026-09-10 — Homebrew's
rustup links only `rustup`, and `~/.cargo/bin` had no shims. `rq/tools` fixed
it as a side effect: its `rustup component add clippy rustfmt` regenerated the
full proxy set. If it ever recurs, the symptom is `command -v cargo` finding
nothing while `~/.rustup/toolchains/*/bin/cargo` exists, and the fix is
`rustup default stable`.

## Non-negotiable

- **The engine gets no I/O and no clock** (D8). `Instant::now()` belongs in
  `events.rs` and nowhere else — `grep -rn 'Instant::now' src/ | grep -v test` should
  show only `events.rs`. (`engine.rs` has one documented origin helper inside
  its `#[cfg(test)]` module; tests may read the clock, the loop may not.) This
  is what keeps the test suite deterministic and sub-second.
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
