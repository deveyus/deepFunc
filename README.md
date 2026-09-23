# deepFunc

Give an LLM exactly the calling context it needs — no more, no less.
deepFunc takes a function in a Rust, Python, or Go workspace, walks the
language server's call hierarchy two levels up, and emits one markdown
document: **full bodies at depth 1, signatures at depth 2.**

```bash
deepfunc --project ~/src/myapp --lang rust --target 'crate::net::dial'
```

````markdown
# deepFunc: `crate::net::dial`

## crate::ui::connect (src/ui.rs:41-48)

```rust
pub fn connect(addr: &str) {
    dial(addr);
}
```

callers of `crate::ui::connect` (depth 2, signatures):

- `crate::main::main` (src/main.rs:12) — `fn main()`
````

## Status

| Language   | Server                   | State                              |
|------------|--------------------------|------------------------------------|
| Rust       | rust-analyzer            | Full bodies at depth 1             |
| Python     | pyright                  | Full graph; module-scope callers show the call line |
| Go         | gopls                    | Full graph; declaration-line spans |
| TypeScript | typescript-language-server | Blocked (E08): the server never answers post-load requests |

C++ is not planned.

## Install

You need two binaries (`deepfunc-cli` for the terminal, `deepfunc-mcp`
for agents) plus one provisioned language server per language you use.
deepFunc fetches the servers itself — no `go`, `npm`, or `curl`
required.

### Option A — prebuilt binaries

Grab the archive for your platform from
[GitHub releases](https://github.com/deveyus/deepFunc/releases):

| Platform | Archive | Notes |
|----------|---------|-------|
| Linux x86_64 | `deepfunc-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` | Fully static; runs anywhere |
| Windows x86_64 | `deepfunc-vX.Y.Z-x86_64-pc-windows-msvc.zip` | Builds + unit tests pass in CI; live-server paths not yet exercised on real Windows — report what you find |
| macOS | — | Unsupported: `provision` refuses darwin, and there is no CI coverage. The code is platform-branched and *should* work, but nobody has proven it |

```bash
# Linux
tar xzf deepfunc-*.tar.gz
sudo install -m755 deepfunc-cli deepfunc-mcp /usr/local/bin/
```

On Windows, unzip next to each other (e.g. `C:\tools\deepfunc\`) and
add that folder to `PATH` — the MCP server finds the CLI on `PATH`,
or via a `DEEPFUNC_BIN` environment variable pointing at it.

### Option B — build from source

```bash
cargo build --release -p deepfunc-cli -p deepfunc-mcp
```

### Provision the language servers

```bash
deepfunc-cli provision --all        # everything
deepfunc-cli provision --lang rust  # or one language: rust, python, go
```

This downloads pinned servers, hash-verified over TLS, into
`~/.local/share/deepfunc/servers` on Linux and
`%LOCALAPPDATA%\deepfunc\servers` on Windows (plus a shared node
runtime for the JS-based servers). Re-run any time to repair or
upgrade. On NixOS, rust-analyzer cannot run as a generic-linux
binary, so provision points you at nix instead of wasting the
download. See `DESIGN.md` §2d for sources and pins.

Two honest expectations: the first `callers` on a project waits out
the language server's index load (30–60s+ on large workspaces — that
is the server, not deepFunc), and each resident server holds real
memory (a rust-analyzer holding measured ~1.6 GB RSS). The MCP
`acquire` call reports both numbers so you can decide how long to
keep a session warm.

## Usage

```bash
# Dotted or double-colon paths
deepfunc --project . --lang python --target 'pkg.mod.connect'
deepfunc --project . --lang rust --target 'crate::net::dial'

# File:line targets (any position in or near the function)
deepfunc --project . --lang go --target 'main.go:9'

# Write to a file, fail CI-style when nothing calls it
deepfunc --project . --target 'crate::dead::code' --out ctx.md --fail-if-empty

# Override the server binary (provision wrappers carry their own args)
deepfunc --project . --lang python --server-bin /path/to/pyright-langserver --stdio
```

## MCP

deepFunc ships an MCP server (`deepfunc-mcp`) for agents, with four
tools: `acquire` (warm a project and pin it for a TTL you choose),
`callers` (the report, served from the warm holding), `release`
(drop it early), and `provision` (fetch servers). With opencode,
pointing at release binaries on your `PATH`:

```json
{
  "mcp": {
    "deepfunc": {
      "type": "local",
      "command": ["deepfunc-mcp"],
      "enabled": true
    }
  }
}
```

See `dev-scripts/deploy.sh` for the dev/prod split behind the
author's own setup, which keeps server startup in milliseconds.
`acquire` takes 30–60s+ for language server load — raise the client
MCP timeout (`experimental.mcp_timeout`) past that, then enjoy warm
`callers` after.

## Errors

Failures are typed, stable, and rustc-shaped: what went wrong, the
evidence (`note:`), and the suspected fix first (`help:`). No silent
fallbacks — a wrong answer is worse than none.

| Code | Meaning |
|------|---------|
| E01  | Language server missing (`deepfunc provision --lang …`) |
| E02  | No workspace marker walking up from `--project` |
| E03  | Bad target or flag syntax |
| E04  | Target not found after the readiness wait |
| E06  | Server request failed or timed out |
| E07  | Filesystem failure with path context |
| E08  | Language wired but known-broken |

## Development

`DESIGN.md` is authoritative; `AGENTS.md` is the workflow guide.
Three crates: `deepfunc-core` (pure logic), `deepfunc-cli` (LSP driver),
`deepfunc-mcp` (rmcp stdio server). No panics, no unsafe, workspace lints
deny both.

## License

LGPL-3.0-or-later. See `LICENSE`.

## For agents

Paste into your `AGENTS.md` when deepFunc is installed:

```markdown
## deepFunc (caller context)

Lifecycle: `acquire` once per workspace, `callers` per question,
`release` when done (holdings self-expire after TTL otherwise).
- `acquire`: first call pays index load (30–180s, set `timeout_secs`
  past it); reports server RSS + system memory. TTL `300` for one
  lookup, `1800` for active work; re-acquire extends.
- `callers`: needs a live holding — "no holding" means acquire first.
  Target: `path::to::fn` or `file.ext:line`; pass `lang` explicitly
  (rust default, also python/go). Empty report is an answer, not an error.
- `E01` (server missing) → `provision` the lang once, then acquire.
- Each holding is a live server (~1.6 GB RSS for rust-analyzer);
  don't hold workspaces you stopped using.
```
