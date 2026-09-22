#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# deepFunc deterministic iai-callgrind instruction-count benchmarks.
#
# iai-callgrind measures machine-independent instruction counts (Ir),
# which gate performance regressions without wall-clock noise. The
# companion `iai-callgrind-runner` binary must be on PATH (cargo-
# installed, version pinned to match the lib in Cargo.toml). The flake
# devshell puts ~/.cargo/bin on PATH.
#
# Saved baselines live under target/iai/ (names allow only [A-Za-z0-9_]):
#   base_v1  - first baseline, recorded at the first successful run
#
# Usage:
#   dev-scripts/bench.sh                             # plain run, prints Ir
#   dev-scripts/bench.sh --baseline=base_v1          # compare to a baseline
#   dev-scripts/bench.sh --save-baseline=base_v1     # record a new baseline
if ! command -v iai-callgrind-runner >/dev/null 2>&1; then
  echo "iai-callgrind-runner not found on PATH." >&2
  echo "Install it with: cargo install iai-callgrind-runner --version 0.16.1" >&2
  exit 1
fi
cargo bench --bench core -- "$@"
