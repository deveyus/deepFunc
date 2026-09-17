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

## 6. Open items

- LSP client crate choice (lsp-server vs tower-lsp vs direct JSON-RPC).
- Fallback to syn/ast-grep when rust-analyzer fails.
- Signature extraction strategy (rust-analyzer hover vs syn parse).
