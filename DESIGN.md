# deepFunc — DESIGN

## 1. Purpose

CLI takes a Rust workspace and a function path. It emits one markdown file with caller context for an LLM. Depth 1: full bodies. Depth 2: signatures plus call sites only.

Out of scope: cross-language support, exact macro expansion, IDE integration.

## 2. Components

Two crates: `deepfunc-core` (lib, pure call-graph logic + formatting) and `deepfunc-cli` (bin, rust-analyzer LSP driver + file IO).

Core stays pure for testability. CLI owns LSP startup, workspace discovery, and output writing.

## 3. Request flow

CLI resolves target via `textDocument/prepareCallHierarchy`. Then it requests `callHierarchy/incomingCalls` for depth 1, repeats once for depth 2. Core formats the tree into markdown with file:line headers.

Error paths: rust-analyzer missing, target not found, project does not check. Each returns non-zero with a short message.

## 4. Data model

In-memory graph: node = { def path, file, line range, signature, body }. Edge = call site { file, line }. No persistent storage.

Output: single markdown file to stdout or `--out`.

## 5. Invariants

No cycles in output (visited set by def ID). Depth 2 never includes bodies. Every included item carries file:line.

## 5b. Error model (rustc-grade, binding)

Every failure prints one `error[EXX]:` block to stderr and exits non-zero. Format mirrors rustc:

```text
error[E02]: workspace root not found
 --> cwd: /tmp/foo
  |
  | note: walked up 4 parents, found no Cargo.toml
  | help: suspected cause: wrong --project dir. Run from inside the workspace or pass --project /path/with/Cargo.toml
```

Rules:

- Code: stable `E01`–`E07` (see below). Never renumber.
- `what went wrong`: one line, specific values (paths, names, counts).
- `note:`: evidence observed (what was checked, what was found).
- `help:`: corrective action. Use `suspected` language unless the cause is certain. List the most likely fix first, max three steps.
- No panics. No bare `unwrap`/`expect`. IO errors map into typed variants with path context.

Codes:

- `E01` rust-analyzer binary not found on PATH (`--ra-bin` override, install via rustup component).
- `E02` workspace root not found (no Cargo.toml walking up from --project).
- `E03` bad target syntax (want `path::to::fn` or `file.rs:line`; echo what was received).
- `E04` target definition not found (prepareCallHierarchy empty; suspected: stale check, cfg-gated, trait impl — suggest `cargo check` first).
- `E05` no callers found (not an error exit by default; emit empty report with note; `--fail-if-empty` flips to error).
- `E06` rust-analyzer request failed or timed out (include request name, timeout secs; suspected: RA still indexing — suggest retry with --timeout).
- `E07` file read or output write failed (include path and OS error).

## 6. Open items (updated: cold-index race solved)

Solved, with evidence:

- `serverStatus/quiescent` never arrives on a bare stdio client (verified:
  60s probe shows only `workspace/diagnostic/refresh`). It is tracked when
  present but NOTHING gates on it.
- Gate is index stability: `workspace/symbol` polls until the answer is
  non-empty AND unchanged across two polls, or 45s budget. A stable index
  means empty hierarchy answers are genuine, not races.
- RA answers `null` (not `[]`) while indexing and for unresolvable
  positions. All three decoders (`workspace/symbol`, `prepare`, incoming)
  treat null as empty.
- RA `prepareCallHierarchy` resolves identifier positions but returns []
  for mid-body positions (verified raw). `file:line` targets therefore map
  to a name locally (`fn_name_near`: same line, else below for docs/attrs,
  else above for bodies) and reuse the symbol path filtered to the file.
- Workspace root is lexically normalized (trailing `/.` poisoned
  hand-built `file://` URIs).
- Per-query retry backstops remain: 20s on empty `prepare`/incoming while
  not quiescent.

Remaining:

- Per-invocation RA spawn pays full workspace load each run (~30s here).
  No daemon per operator decision.
- Readiness/liveness split: `ensure_index_ready` gates on canary `"a"`
  (non-empty + stable count, 30s budget); target lookups then trust empty
  after one 3s retry. Unknown names fail in load-time + ~3s. Hierarchy
  empties carry a 5s backstop only. A workspace with no `a`-matching
  function names would burn the canary budget and proceed unguarded
  (accepted: vanishingly rare, still correct, just slower).
