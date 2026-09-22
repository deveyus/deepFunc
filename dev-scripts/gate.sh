#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# deepFunc commit gate. Run before every commit (see AGENTS.md).
# One devshell session, every step, all failures reported, exit non-zero
# if any step failed.
#
# Conditional steps are NOT here: bench (hot-path changes) and fuzz
# (wire/protocol parser changes) are run on demand, per AGENTS.md.

fail=0
step() {
  local name="$1"
  shift
  echo "=== $name ==="
  if "$@"; then
    echo "PASS: $name"
  else
    echo "FAIL: $name"
    fail=1
  fi
}

step check     cargo check --all-targets
step clippy    cargo clippy --all-targets
step fmt       cargo fmt --check
step test      cargo test
step test-rel  cargo test --release
# Coverage gate applies to the pure library only (see DESIGN.md §16):
# binaries are IO/LSP/network-bound and need integration tests, tracked
# as an open item. Full-workspace report still prints for visibility.
step coverage  cargo llvm-cov --package deepfunc-core --fail-under-lines 90
step verify    bash "$REPO_ROOT/dev-scripts/verify.sh"

if [ "$fail" -ne 0 ]; then
  echo "GATE: FAIL"
  exit 1
fi
echo "GATE: PASS"
