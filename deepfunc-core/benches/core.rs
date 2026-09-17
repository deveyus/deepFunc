//! Deterministic instruction-count baselines (`cargo bench --bench core`).
//!
//! Counts are machine-independent, so they gate regressions without
//! the wall-clock noise that plagues build-time measurements.
//!
//! Placeholder until the hot paths (sanitizer, password generator,
//! wire framing) exist and get their own benches.

use iai_callgrind::{library_benchmark, library_benchmark_group, main};

/// Fixed 4 KiB scan buffer standing in for child output.
const SCAN: &[u8] = &[0x61; 4096];

// Single linear pass over the buffer, as the sanitizer's exact-match
// tier will do per credential. (iai: no doc comments on benchmarks.)
#[library_benchmark]
fn placeholder_scan() -> usize {
    let needle = b"placeholder";
    SCAN.windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

library_benchmark_group!(
    name = core_hot_paths;
    benchmarks = placeholder_scan
);

main!(library_benchmark_groups = core_hot_paths);
