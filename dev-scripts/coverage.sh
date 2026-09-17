#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# cargo-llvm-cov needs RUST_SRC_PATH, which the flake devshell sets.
# Extra args pass through, e.g.:
#   dev-scripts/coverage.sh --fail-under-lines 90
cargo llvm-cov "$@"
