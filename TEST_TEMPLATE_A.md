# Subagent A — Property Extraction

Read the function at `<file>:<line-range>`, then extract ALL properties
that should be tested.

For each property, list:
- The invariant (what must always be true)
- The inputs that exercise it
- The expected output

Include ALL of the following categories:

1. **Round-trip / inverse relationships** — e.g., encode(decode(x)) == x, read-after-write
2. **Identity / idempotence** — same input gives same output; or explicitly noted when not
3. **Bounds, limits, overflow/underflow** — max/min values, type casts, wrapping behavior
4. **Determinism (or lack thereof)** — same input → same output? Note sources of non-determinism (UUID, clocks, RNG, allocator)
5. **Invariants** — things that must always be true regardless of input
6. **Totality** — does the function ever panic? On what inputs? (even via transitive dependency panics)
7. **Resource safety** — temp file cleanup, connection drops, locks released on all paths
8. **Any other property** you can identify

Be exhaustive. List EVERY property even if it seems obvious.
Do NOT include inlined code — use file:line-range references only.
