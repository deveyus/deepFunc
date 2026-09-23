# deepFunc for agents

Caller context for one function: full bodies of its callers, signatures
of their callers. Lifecycle is `acquire → callers → release`.

## Session pattern

1. `acquire` the workspace once. First call pays index load (30–180s —
   set `timeout_secs` past it). It reports server RSS + system memory.
2. `callers` per question. Warm calls are fast; the holding keeps the
   server resident between your calls.
3. `release` when done early. Otherwise holdings self-expire `ttl_secs`
   after last use.

## TTL picks

- `300` — one lookup, then moving on.
- `1800` — active work in this workspace.
- Re-acquire anytime to extend; last use resets the clock.

## Target syntax

`path::to::function`, `path.to.function`, or `file.ext:line` (any
position in or near the function). `lang` defaults to rust; pass
`python` or `go` explicitly — auto-detection is not attempted.

## When it fails

- "no live holding" → `acquire` first, then retry. Never worked around.
- `E01` (server missing) → `provision` that lang once, then `acquire`.
- Empty report ("no callers found") is an answer, not an error — unless
  `--fail-if-empty` semantics were requested.
- Timeouts keep the holding; retry with a larger `timeout_secs`.

## Costs to respect

Each holding is a live language server: ~1.6 GB RSS measured for
rust-analyzer on a mid-size workspace. Check the `acquire` memory
lines before holding three workspaces at once.
