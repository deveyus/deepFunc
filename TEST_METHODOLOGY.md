# Test Methodology

> **Why three agents?** Coding biases what you think is worth testing.  You
> naturally focus on the dark corners you deliberately handled and overlook
> the implicit edges you didn't think about.  Asking *blind* subagents
> — one for properties, one for control flow, one for adjacent callers —
> *after* the code is written gives you the unpolluted specification
> independently of the implementation.  For anything beyond utterly trivial
> glue functions, this separation catches gaps you would not see otherwise.

Each significant function or trait method should be analyzed by three
independent subagents when generating its tests.  This provides a
specification-coverage lens (what should hold), an implementation-coverage
lens (what paths exist), and an integration lens (how callers and callees
interact).

## Workflow

```
Already-written function (referenced as file:line-range only — never inline code)
        │
        ├──→ Subagent A (read TEST_TEMPLATE_A.md, analyze function)
        │
        ├──→ Subagent B (read TEST_TEMPLATE_B.md, analyze function)
        │
        ├──→ Subagent C (read TEST_TEMPLATE_C.md, analyze function)
        │
        └──→ Developer: write tests using all three outputs
```

## Subagent A — Property Extraction

Read `TEST_TEMPLATE_A.md` for the full prompt.  
Core instruction: analyze `<file>:<line-range>`, list all properties.

## Subagent B — Control Flow Extraction

Read `TEST_TEMPLATE_B.md` for the full prompt.  
Core instruction: analyze `<file>:<line-range>`, list all control flow paths.

## Subagent C — Adjacent Analysis

Read `TEST_TEMPLATE_C.md` for the full prompt.  
Core instruction: analyze `<file>:<line-range>`, check caller/callee boundaries.

## Writing Tests

The property list from Subagent A drives `proptest!` blocks:

```rust
proptest! {
    #[test]
    fn decode_never_panics(data in any::<u8>()) {
        let _result = decode_message(&data); // must be total
    }
}
```

The control flow list from Subagent B drives named tests for each
specific path — especially error paths that proptest may not hit
efficiently.

The boundary analysis from Subagent C drives contract tests that
exercise the call chain with values that stress each interface:

```rust
#[test]
fn zero_payload_len_rejected() {
    let data = [0, 0, 0, 0];
    let err = decode_message(&data).unwrap_err();
    assert!(matches!(err, ProtocolError::BufferTooShort { .. }));
}
```

## Coverage Target: 90%

All code merged to main must maintain ≥90% line and branch coverage,
measured by `cargo-llvm-cov`.  This is verified in CI.

Exceptions are noted with `// #[cfg(not(tarpaulin))]` or gated behind
feature flags and documented in the PR.  Every exception requires a
justification in the review.

Control-flow-heavy code (wire parsers, protocol handlers, module
dispatch) must be measured separately with fuzz targets
(`cargo-fuzz`).  Fuzz targets live in each crate's `fuzz/`
directory and are run for a minimum of 60 CPU-seconds per CI run.

## When to Apply (post-implementation)

Apply this methodology (all three subagents) to every non-trivial
function *after* the code is written, as part of the test-writing pass:

- Every public API function
- Every trait method with documented contract
- Every function that handles untrusted input (wire format, I/O)
- Functions with multiple branches or error paths

## Exceptions

Trivial accessors, constructors with no branching, and functions
that delegate entirely to a tested sub-function do not need this
treatment.

## Important Rule

**Never inline the function code in prompts to subagents.**  
Always provide the file:line-range reference and let the subagent
read it themselves.  This ensures the subagent sees the exact code
as it exists on disk, not a potentially stale copy embedded in the
prompt.

**Use absolute file paths** in subagent prompts.  Subagents run
with a working directory of `/tmp/opencode` and cannot resolve
relative paths like `oracle-server/src/foo.rs`.  Always prefix
with the full project path, e.g.:
`oracle-server/src/http_poem/download.rs:45-105`.
