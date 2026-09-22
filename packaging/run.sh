#!/usr/bin/env bash
# deepFunc MCP server launcher (production).
#
# No nix devshell: release binaries live in bin/ (see
# ~/dev/deepFunc/dev-scripts/deploy.sh), rust-analyzer and the cargo
# toolchain are gc-rooted nix store symlinks beside this file. Everything
# resolves in milliseconds so MCP tool registration never races startup.
# The language server needs cargo on PATH for workspace introspection;
# the toolchain-* links provide it without a devshell.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ ! -x "$HERE/bin/deepfunc-mcp" ]]; then
  echo "deepfunc MCP: $HERE/bin/deepfunc-mcp missing (run ~/dev/deepFunc/dev-scripts/deploy.sh)" >&2
  exit 1
fi

export DEEPFUNC_BIN="$HERE/bin/deepfunc-cli"
export PATH="$HERE/bin:$HERE/rust-analyzer/bin:$HERE/toolchain-cargo/bin:$HERE/toolchain-rustc/bin:$PATH"
exec "$HERE/bin/deepfunc-mcp"
