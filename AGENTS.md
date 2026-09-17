# Template — Agent Workflow Guide

## Task Tracking

Vikunja via skill `vikunja`. Load the skill first.
Board: project `Template` (five-bucket kanban). Labels are advisory; buckets carry state.

## Every Session

1. Read `DESIGN.md` (authoritative) and this file.
2. `git log --oneline -5` for current state.
3. `dev-scripts/check.sh` while iterating; `dev-scripts/gate.sh` before commit.

## Stack

| Layer | Choice |
|-------|--------|
| Language | Rust 2021, `#![forbid(unsafe_code)]` |
| Build | Nix flake devshell (`nix develop .`) |
| Settings | `config.db` (SQLite) if needed |
| Audit | `audit_log.db` (SQLite) if needed |
| Secrets | per-project `secrets.yaml`, SOPS + age if needed |
| Panic check | `no-panic` dev-dep, `cargo test --release` |
| Formal | Creusot (pure logic) + Kani (IO seams) — see below |
| Perf | iai-callgrind 0.16, `Ir` counts, `[[bench]] harness = false` |
| Fuzz | `cargo-fuzz`, `fuzz/` per crate, 60s default |

## Development tools — dev-scripts/

Each script self-locates into the flake devshell, runs from the repo root, and has a reliable exit code. `common.sh` is sourced, never executed.

| Script | Purpose |
|--------|---------|
| `check.sh` | `cargo check --all-targets` |
| `clippy.sh` | `cargo clippy --all-targets` |
| `fmt.sh` | `cargo fmt` (apply; gate runs `cargo fmt --check`) |
| `test.sh` | `cargo test` |
| `test-release.sh` | `cargo test --release` — link-time no-panic check |
| `coverage.sh` | `cargo llvm-cov`; args pass through |
| `verify.sh` | Kani + Creusot; SKIP gracefully when a binary cannot run |
| `bench.sh` | iai-callgrind; `--save-baseline` / `--baseline` pass through |
| `fuzz.sh` | `cargo fuzz run <package> <target>` — 60s default |
| `gate.sh` | commit gate: check, clippy, fmt --check, test, test-release, coverage, verify |
| `init.sh` | one-shot template init (idempotent, handles FHS/Kani) |

Bench and fuzz are not in the gate. Run them on demand per Commit Discipline.

## Commit Discipline

- One logical change per commit. Message: `template: <what changed>` or `<area>: <what>`.
- `dev-scripts/gate.sh` before every commit. Red gate blocks the commit.
- Coverage gate is 90% (`coverage.sh --fail-under-lines 90`). It is red on the current scaffold (no tests yet) by design.
- Conditional, on demand:
  - Hot path: `dev-scripts/bench.sh` vs saved baseline. `Ir` regression blocks the commit.
  - Wire/protocol parser: `dev-scripts/fuzz.sh <package> <target>`.
- Never commit: `target/`, runtime dbs, secrets.

## Code Style

- `#![forbid(unsafe_code)]` in every crate — non-negotiable.
- No panics in our code. Workspace lints deny `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`. Return `Result`; do not add panic paths. Third-party crates may panic internally.
- Leaf logic gets `#[cfg_attr(not(debug_assertions), no_panic::no_panic)]` in `tests/no_panic.rs`, checked by `dev-scripts/test-release.sh`.

## Testing — three-agent methodology

Follows `TEST_METHODOLOGY.md` (templates `TEST_TEMPLATE_A.md`/`B`/`C`) in this repo — copied from furnace. After code is written, three blind subagents analyze each non-trivial function by `file:line-range` only (absolute paths, never inlined code):

- A — properties (`proptest!` totality, round-trip, invariants).
- B — control flow (per-path tests, especially error paths).
- C — caller/callee boundaries (contract tests).

Coverage ≥90% line and branch. Parsers and wire format get fuzz targets.

## Formal verification — 90/10

Keep the pure core isolated so the IO seam stays small. If a function needs more than seam isolation, that is a smell.

- **90% — Creusot** (`cargo creusot prove`, `creusot-std` pinned to furnace rev `2a1bc72`): `#[requires]`/`#[ensures]` on pure core.
- **10% — Kani** (`kani` + `cargo-kani` 0.67, needs `cbmc` and `steam-run` on NixOS): the IO seams behind a small `System` trait with a mock. Harnesses live in `src/harness.rs` as `#[cfg(kani)] #[kani::proof] #[kani::unwind(20)]`. Unwind `20` is the default (matches furnace `ember/src/harness.rs:11,31`). Bump per harness only if a loop needs it and state the bound in a comment.
- Run via `dev-scripts/verify.sh`. On NixOS invoke Kani through `steam-run` (`steam-run cargo kani`). The script SKIP is normal when a toolchain is missing.

## Performance — callgrind auditing

- iai-callgrind 0.16, `[[bench]] harness = false`, benches in `template-core/benches/`.
- Baselines under `target/iai/` (`bench.sh --save-baseline=X` / `--baseline=X`). `base_v1` is the placeholder; re-baseline when real hot paths land. Numbers live in `DESIGN.md` §16.
- Any hot-path change ends with `dev-scripts/bench.sh` compared to the baseline. `Ir` regression blocks the commit.

## Environment constraints

- Rust comes only from the flake devshell; `dev-scripts/` re-execs into it.
- Python is not available. `sed`/`awk`/`perl` are banned — use `Edit`/`Write`.
