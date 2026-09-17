#!/usr/bin/env bash
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh" "${BASH_SOURCE[0]}" "$@"
# keyServ formal verification — Creusot deductive + Kani BMC.
# Mirrors furnace/scripts/verify-track-b.sh.
#
# Always runs cargo check; runs kani/creusot when runnable, skips with a
# note otherwise.
#
# Kani on NixOS: cargo-installed kani-verifier hardcodes an FHS layout
# (glibc/libstdc++ at /lib64) that the bare NixOS stub-ld rejects
# (https://nix.dev/permalink/stub-ld). Even a local `cargo install` of
# kani-verifier hits the stub — you cannot fix it by recompiling locally.
# furnace fixes this by running Kani inside steam-run's FHS bubblewrap
# (furnace/shell.nix adds pkgs.steam-run + pkgs.cbmc and probes
# `steam-run cargo kani`). This script mirrors that — try steam-run first,
# then bare cargo kani, then SKIP. Do not patchelf the ~/.cargo/bin binaries
# in place.
#
# steam-run mounts /tmp as a fresh tmpfs, so the host's $TMPDIR
# (/tmp/nix-shell.*) disappears inside the FHS. Rust needs TMPDIR for
# temp files; set TMPDIR=/tmp for the FHS invocation. Also the rustup
# nightly's gcc-ld wrapper hardcodes the nix store path of rustup at
# install time — after a `nixpkgs` update the path goes stale (e.g.
# 0l25… vs yhflfa…). If Kani fails with
# ".../ld-wrapper.sh: No such file", reinstall the toolchain:
#   nix-shell -p rustup --run 'rustup toolchain uninstall nightly && rustup toolchain install nightly'
export PATH="$HOME/.cargo/bin:$PATH"
echo "=== keyServ formal verification ==="
echo "--- cargo check ---"
cargo check -p keyserv-core -p keyserv-exec -q
echo "--- kani (BMC) ---"
# Probe in FHS first (furnace fix), then bare.
if TMPDIR=/tmp steam-run cargo kani --help >/dev/null 2>&1; then
  echo "kani via steam-run: $(TMPDIR=/tmp steam-run cargo kani --version 2>&1 | head -1)"
  TMPDIR=/tmp steam-run cargo kani --package keyserv-core
  TMPDIR=/tmp steam-run cargo kani --package keyserv-exec
elif cargo kani --help >/dev/null 2>&1; then
  echo "kani bare: $(cargo kani --version 2>&1 | head -1)"
  cargo kani --package keyserv-core
  cargo kani --package keyserv-exec
else
  echo "SKIP kani (cargo-kani not runnable; expected at ~/.cargo/bin/cargo-kani, try steam-run cargo kani)"
fi
echo "--- creusot (deductive) ---"
# --help exits 0 even when the creusot toolchain is missing, so probe by
# running and catching the specific "not installed" error: that skips, any
# other failure is a real proof failure and fails the script.
if output=$(cargo creusot prove --package keyserv-core 2>&1); then
  printf '%s\n' "$output"
elif printf '%s' "$output" | grep -q "creusot-rustc not found"; then
  echo "SKIP creusot (toolchain not installed; expected at ~/.local/share/creusot)"
else
  printf '%s\n' "$output" >&2
  exit 1
fi
echo "PASS"
