#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# Deny warnings: CI runs with -D warnings, so the gate must too.
# (A missing deny here let an unused_mut in a test reach main twice.)
cargo clippy --all-targets -- --deny warnings
