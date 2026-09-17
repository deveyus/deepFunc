#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# Release build: this is where the no-panic link-time verification runs
# (tests/no_panic.rs, cfg_attr-gated to non-debug builds).
cargo test --release
