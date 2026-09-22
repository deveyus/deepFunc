#!/usr/bin/env bash
# deepFunc deploy: dev -> production split.
#
# Development lives in this repo (flake devshell, cargo, gate).
# Production is ~/mcp/deepfunc/: release binaries, run.sh, and gc-rooted
# nix store paths (rust-analyzer + cargo toolchain). NO devshell anywhere
# near serving: run.sh must start in milliseconds so MCP tool registration
# (5s default fetch timeout) never races nix evaluation. The cargo
# toolchain ships because rust-analyzer needs `cargo metadata` for
# workspace introspection even though deepfunc never shells out to cargo.
#
# Usage: ./dev-scripts/deploy.sh [--skip-build]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROD="$HOME/mcp/deepfunc"
BIN="$PROD/bin"

cd "$REPO_ROOT"

if [[ "${1:-}" != "--skip-build" ]]; then
  if ! command -v cargo >/dev/null 2>&1; then
    exec nix develop . --command bash "$0" "$@"
  fi
  cargo build --release -p deepfunc-cli -p deepfunc-mcp
fi

mkdir -p "$BIN"
cp -f target/release/deepfunc-cli target/release/deepfunc-mcp "$BIN/"
cp -f packaging/run.sh "$PROD/run.sh"
chmod +x "$PROD/run.sh"
git rev-parse HEAD > "$PROD/VERSION"

# gc-rooted nix store paths (stable symlinks, survive GC).
# Re-run deploy if a pinned version ever changes.
if [[ ! -x "$PROD/rust-analyzer/bin/rust-analyzer" ]]; then
  nix build --out-link "$PROD/rust-analyzer" "nixpkgs#rust-analyzer"
fi
if [[ ! -x "$PROD/toolchain-cargo/bin/cargo" ]]; then
  nix build --out-link "$PROD/toolchain-cargo" "nixpkgs#cargo"
fi
if [[ ! -x "$PROD/toolchain-rustc/bin/rustc" ]]; then
  nix build --out-link "$PROD/toolchain-rustc" "nixpkgs#rustc"
fi

export DEEPFUNC_BIN="$BIN/deepfunc-cli"
export PATH="$PROD/rust-analyzer/bin:$PROD/toolchain-cargo/bin:$PROD/toolchain-rustc/bin:$PATH"
echo "deepfunc deployed: $(cat "$PROD/VERSION")"
"$BIN/deepfunc-cli" --help 2>&1 | head -1 || true
"$PROD/rust-analyzer/bin/rust-analyzer" --version
cargo --version
