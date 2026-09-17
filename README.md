# Template — Rust Template

A strict, batteries-included Rust workspace. Nix flake, formal verification, deterministic benches, and a one-shot init.

## Quick start

```bash
# clone as your new project
git clone file:///home/serafina/dev/rust-template myapp
cd myapp

# stamp project name, author, year, description (idempotent, dry-run first)
dev-scripts/init.sh --dry-run --project myapp --title MyApp --author "Your Name" --year 2026
dev-scripts/init.sh --project myapp --title MyApp --author "Your Name" --author "Your Name" --email you@example.com

# enter devShell and verify
nix develop . --command bash dev-scripts/gate.sh
```

`init.sh` is idempotent and resumable. It stamps `Cargo.toml` (workspace + crates), `flake.nix` description, `LICENSE` copyright, `DESIGN.md` title, `AGENTS.md` board, and `src` headers. It also ensures `cargo-llvm-cov`, `cargo-fuzz`, `iai-callgrind-runner`, `why3`, `z3`, `cbmc`, `steam-run`, `kani`, and `creusot` are present.

## What you get

- **Workspace** `template-core` (lib) + `template-exec` (bin) — rename via `init.sh`
- **Lints** `#![forbid(unsafe_code)]`, `unwrap_used/expect_used/panic = deny`, `no-panic` checked in `tests/no_panic.rs`
- **Verification** Creusot `creusot-std 2a1bc72` for pure logic, Kani `0.67` for IO seams via `steam-run` FHS (NixOS stub-ld fix), `why3/z3` in flake
- **Perf** `iai-callgrind 0.16` `Ir` counts, baselines under `target/iai/`
- **Fuzz** `cargo-fuzz` 60s per target
- **Gate** `dev-scripts/gate.sh` aggregates `check, clippy, fmt --check, test, test-release, coverage, verify`
- **Docs** `TEST_METHODOLOGY.md` + `TEST_TEMPLATE_A/B/C.md` copied from `furnace` in-repo (see `AGENTS.md: Testing`)

## Kani on NixOS

Kani hardcodes FHS glibc paths and hits the stub-ld even after local `cargo install`. This template mirrors `furnace/shell.nix`: `flake.nix` adds `pkgs.steam-run + pkgs.cbmc` (`allowUnfree`), `verify.sh` probes `TMPDIR=/tmp steam-run cargo kani` first. Do not `patchelf` `~/.cargo/bin`. See `dev-scripts/init.sh` and `dev-scripts/verify.sh` comments.

## Creusot

`cargo-creusot` is installed via `cargo install --git https://github.com/creusot-rs/creusot`. Its toolchain lives at `~/.local/share/creusot` and needs `cargo creusot setup install` (800 MB) + `why3 config detect`.

## Template philosophy

`DESIGN.md` is authoritative, `AGENTS.md` is the workflow guide. Keep `dev-scripts/` honest. `init.sh` is the only file that knows about `Template → YourProject` renaming.

## License

LGPL-3.0-or-later. See `LICENSE`. `init.sh` stamps your name/year there.
