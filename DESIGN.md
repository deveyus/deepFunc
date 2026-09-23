# deepFunc — DESIGN

## 1. Purpose

CLI takes a Rust workspace and a function path. It emits one markdown file with caller context for an LLM. Depth 1: full bodies. Depth 2: signatures plus call sites only.

Out of scope: cross-language support, exact macro expansion, IDE integration.

## 2. Components

Three crates: `deepfunc-core` (lib, pure call-graph logic + formatting),
`deepfunc-cli` (bin, multi-language LSP driver + file IO), `deepfunc-mcp`
(bin, rmcp stdio server with one `callers` tool that shells out to the CLI
via `DEEPFUNC_BIN`). Core stays pure. CLI owns LSP. MCP owns protocol.

Result shapes follow rmcp's guidance: success is unstructured text
(no `structuredContent`, which chokes record-expecting clients);
CLI failures (the typed E01–E08 errors) come back as TOOL-level errors
so the diagnostics stay visible instead of rendering as opaque -32603.
Only infrastructure failures (spawn, join, timeout) are protocol errors.

## 2b. Languages (rust, python, go; typescript blocked)

One `Language` table in `deepfunc-cli`: server command, LSP language ID,
workspace markers, source extensions, install hint, and an optional
`broken` flag that fails loudly (E08) instead of guessing. `--lang`
selects; `--server-bin` overrides the program (table args kept).

- rust: rust-analyzer. Full bodies at depth 1 (server spans are complete).
- python: pyright. Module-scope callers report the call-site line as
  their signature (kind Module has none).
- go: gopls. Definition spans are declaration-line narrow; bodies at
  depth 1 are signatures until gopls widens spans or we resolve them via
  documentSymbol ranges.
- typescript: BLOCKED (E08). typescript-language-server 5.3.0 and npm
  6.0.0 never forward tsserver-backed requests once a project loads:
  tsserver's own log shows a healthy configured project loading in ~1s,
  but no navto/hover/prepare command ever arrives (~15 raw probes:
  instant errors pre-load, silence post-load, both versions, fixture and
  real project). Unblocks when a server version answers post-load
  requests; remove the `broken` flag then. C++ excluded by operator
  decision (use Rust).

No scan fallback exists by design: E01/E04/E06/E08 are loud errors.
Server notifications (window/logMessage et al.) are captured and dumped
to stderr on failure, so config errors surface with the typed error.

## 2c. Deployment (dev/prod split)

Development lives in this repo (flake devshell, cargo, gate).
Production is `~/mcp/deepfunc/`, assembled by `dev-scripts/deploy.sh`
(build release, copy binaries, stamp VERSION, gc-root rust-analyzer):
`run.sh` execs in milliseconds with no nix involved, so MCP tool
registration (5s default fetch timeout) never races a devshell.

At serve time the CLI resolves each language server in order:
`--server-bin`, table program on PATH, provisioned manifest. `run.sh`
sets only PATH (for the gc-rooted rust-analyzer) and DEEPFUNC_BIN;
everything else self-resolves, including provisioned copies.

## 2d. Provisioning (`deepfunc provision`)

E01 fails loudly and points here. `provision --lang ID [--version V]
[--dir D]` / `--all` downloads pinned servers with NO helper tools
installed (no go/npm/pip/curl dependency — pure Rust: ureq, flate2+tar,
lzma-rs+tar, sha2/sha1+hex). Hashes verify against authoritative
metadata (npm shasum, go.dev sha256, nodejs SHASUMS256); rust-analyzer
has no published checksums (TLS-only, stated in code). Each install
writes a manifest and prints the `--server-bin` path:

- rust: GitHub release asset (pinned tag, gunzip) + `--version` smoke.
  NixOS refuses generic-linux binaries (stub-ld): provision detects it
  and fails LOUDLY toward nix instead of wasting a 40MB download.
- go: toolchain tarball (pinned, sha256) → `go install gopls@pin` with
  the provisioned toolchain → wrapper pinning GOROOT+PATH (gopls shells
  out to `go list` at serve time, when no Go is around).
- python/typescript: npm registry tarballs (shasum) unpacked natively;
  bin entry resolved from package.json; run under a shared provisioned
  node (or system node on NixOS, where nodejs.org binaries cannot
  execute) via a generated wrapper script.

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

- Code: stable `E01`–`E08` (see below). Never renumber, only append.
- `what went wrong`: one line, specific values (paths, names, counts).
- `note:`: evidence observed (what was checked, what was found).
- `help:`: corrective action. Use `suspected` language unless the cause is certain. List the most likely fix first, max three steps.
- No panics. No bare `unwrap`/`expect`. IO errors map into typed variants with path context.

Codes:

- `E01` language server not found (per-language install hint, or `deepfunc provision --lang`).
- `E02` workspace root not found (no language marker walking up from --project).
- `E03` bad target or flag syntax (echo what was received).
- `E04` target definition not found after the readiness wait.
- `E05` reserved: no callers found is NOT an error exit (empty report with note); `--fail-if-empty` flips it to E04.
- `E06` server request failed or timed out (server name, request, timeout-epilogue with the server's message).
- `E07` filesystem failure with path context.
- `E08` language wired but known-broken (typescript: server never answers post-load requests).

## 6. Open items and solved record

Solved, with evidence:

- `serverStatus/quiescent` never arrives on a bare stdio client (verified:
  60s probe shows only `workspace/diagnostic/refresh`). Tracked when
  present; nothing gates on it.
- Readiness/liveness split: `ensure_index_ready` gates on canary `"a"`
  (non-empty + stable count across polls, 30s budget, errors count as
  loading). Target lookups then trust empty after short retries. Unknown
  names fail in load-time + ~3s.
- Servers answer `null` (not `[]`) while indexing and for unresolvable
  positions. All decoders treat null as empty.
- `prepareCallHierarchy` resolves identifier positions but returns []
  for mid-body positions (verified raw). `file:line` targets therefore
  resolve via `documentSymbol` innermost-callable lookup — no text
  parsing, works in every language.
- Workspace root is lexically normalized (trailing `/.` poisoned
  hand-built `file://` URIs).
- Module-scope callers (pyright) report the call-site line as their
  signature (a module has none).
- Seed `didOpen`: some servers only build a project model once a file is
  open; one seed file opens before querying.

Remaining:

- Coverage gate (90% lines) applies to `deepfunc-core` only. Binaries are
  IO/LSP/network-bound: unit tests cover every pure function (arg
  parsing, path math, symbol-tree walk, archive roundtrips, error
  rendering), but hierarchy walks, downloads, and spawns need live
  servers. Integration tests against fixture workspaces are the tracked
  follow-up; the full-workspace coverage report still prints for
  visibility on every gate run.

## 2e. Holdings (acquire/release/callers)

Per-call spawn cost (~30s load) stood until the daemon decision was
revisited with data. Resolution: no new binary, no new protocol — the
MCP server itself is persistent, so it keeps one live `LanguageClient`
per `(workspace, lang)` across calls. The model manages lifecycle
explicitly; the server enforces bounds:

- `acquire(project, lang?, ttl_secs!, timeout_secs?)`: spawn (or reuse),
  wait for index readiness, pin for TTL seconds after last use.
  `ttl_secs` is REQUIRED (1800 suggested); re-acquire extends.
  Reports server RSS plus system free/total so retention is informed.
- `callers(...)`: serves ONLY from a live holding. No holding, lapsed
  TTL, or dead server fails loudly telling the model to acquire —
  never a silent cold-spawn (it cannot pick a TTL for you).
- `release(project?, lang?)`: drop now (scope to all when omitted).
- TTL expiry sweeps on every call; concurrent access serializes on one
  mutex per holding (single-agent use). Dead children respawn only via
  explicit re-acquire. Memory via `sysinfo` (RSS + total/available).
- The CLI stays spawn-per-call (scripting discipline); holdings live
  only in the MCP server process.
