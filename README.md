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

Prebuilt binaries ride each
[GitHub release](https://github.com/deveyus/deepFunc/releases)
(static Linux musl + Windows MSVC; macOS is unsupported — provision
refuses darwin). Unpack, then provision the pinned language servers:

```bash
./deepfunc-cli provision --all
```

Or build from source. No helpers required — no `go`, `npm`, or
`curl` needed. deepFunc downloads everything itself, hash-verified:

```bash
cargo build --release -p deepfunc-cli
./target/release/deepfunc-cli provision --all
```

This installs pinned language servers into `~/.local/share/deepfunc/servers`
(plus a shared node runtime for the JS servers). On NixOS, rust-analyzer
cannot run as a generic-linux binary, so provision points you at nix
instead of wasting the download. See `DESIGN.md` §2d for sources and pins.

For development (linted, verified, benchmarked), use the Nix flake:

```bash
nix develop . --command bash dev-scripts/gate.sh
```

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

deepFunc ships an MCP server (`deepfunc-mcp`, tools `callers` +
`provision`) for agents. With opencode:

```json
{
  "mcp": {
    "deepfunc": {
      "type": "local",
      "command": ["~/mcp/deepfunc/run.sh"],
      "enabled": true
    }
  }
}
```

See `dev-scripts/deploy.sh` for the dev/prod split that
keeps server startup in milliseconds. Cold calls take 30–60s for language
server load — raise the client MCP timeout (`experimental.mcp_timeout`)
if calls time out.

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
