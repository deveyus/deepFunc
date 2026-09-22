#!/usr/bin/env bash
# deepFunc deploy: dev -> production split.
#
# Development lives in this repo (flake devshell, cargo, gate).
# Production is ~/mcp/deepfunc/: release binaries + a gc-rooted
# rust-analyzer, NO devshell. run.sh must start in milliseconds so MCP
# tool registration (5s default timeout) never races nix evaluation.
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
git rev-parse HEAD > "$PROD/VERSION"

# rust-analyzer: gc-rooted nix store path (stable symlink, survives GC).
# Re-run if the pinned version ever changes.
if [[ ! -x "$PROD/rust-analyzer/bin/rust-analyzer" ]]; then
  nix build --out-link "$PROD/rust-analyzer" "nixpkgs#rust-analyzer"
fi

export DEEPFUNC_BIN="$BIN/deepfunc-cli"
export PATH="$PROD/rust-analyzer/bin:$PATH"
echo "deepfunc deployed: $(cat "$PROD/VERSION")"
"$BIN/deepfunc-cli" --help 2>&1 | head -1 || true
"$PROD/rust-analyzer/bin/rust-analyzer" --version
