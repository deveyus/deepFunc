//! Placeholder Kani harness — a trivial proof so `cargo kani` has a target
//! until core has real invariants to check.

use kani;

/// Wrapping add of zero is identity. Trivially true; replace with real invariants.
#[kani::proof]
#[kani::unwind(20)]
fn harness_scaffold() {
    let n: u8 = kani::any();
    kani::assert(n.wrapping_add(0) == n, "wrapping add zero is identity");
}
