# Shared setup for keyServ dev-scripts. Sourced, never executed.
#
# Each script starts:
#   #!/usr/bin/env bash
#   set -euo pipefail
#   source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}"
#
# After sourcing:
#   - cwd is the repo root
#   - we are inside the flake devshell (re-exec'd in automatically if not)
#   - REPO_ROOT and SELF are set
#
# $1 = calling script path; $2.. = its arguments.
set -euo pipefail

SELF="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
REPO_ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
cd "$REPO_ROOT"

# The flake devshell provides cargo, clippy, rustfmt, cargo-llvm-cov,
# cargo-fuzz, why3, z3, RUST_SRC_PATH, and ~/.cargo/bin on PATH.
if ! command -v cargo >/dev/null 2>&1; then
  exec nix develop . --command bash "$SELF" "${@:2}"
fi
