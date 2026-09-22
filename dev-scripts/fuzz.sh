#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# Fuzz one target: dev-scripts/fuzz.sh <package> <target> [max_total_time]
# Targets live in <package>/fuzz/ (TEST_METHODOLOGY.md). cargo-fuzz
# operates on the crate in the current directory, so we cd there
# (convention: package directory is named after the package).
# Default budget: 60 CPU-seconds, per the methodology.
if [ $# -lt 2 ]; then
  echo "usage: $0 <package> <target> [max_total_time]" >&2
  exit 2
fi
package="$1"
target="$2"
max_total_time="${3:-60}"
if [ ! -d "$REPO_ROOT/$package" ]; then
  echo "no package directory '$package' (expected at $REPO_ROOT/$package)" >&2
  exit 2
fi
cd "$REPO_ROOT/$package"
cargo fuzz run "$target" -- "-max_total_time=$max_total_time"
