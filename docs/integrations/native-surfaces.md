# Native product surfaces

The native `distill` binary exposes the same Rust `Engine` through three thin
surfaces:

- CLI: `project`, `artifact get`, `artifact trace`, `status`, `gc`, `read`, and
  argv-only `run`.
- Codex: `codex-hook --mode off|observe|active`, installed into a versioned
  `PostToolUse` hook with `distill setup codex`.
- Claude Code: stdio MCP with exactly `distill_read` and `distill_run`, installed
  with `distill setup claude`.

Both setup targets require an explicit absolute configuration path, support
`--dry-run`, preserve an exact first-install backup, are idempotent, and restore
that backup with `--restore`.

Codex replacement uses documented `PostToolUse` blocking feedback because
`updatedMCPToolOutput` is parsed but unsupported. The adapter caps feedback at
2,250 tokens against Codex's approximate 2,500-token model-visible hook-output
limit. The versioned matrix is
[`codex-hook-conformance-v1.json`](codex-hook-conformance-v1.json). Hosted tools
and specialized paths that do not emit `PostToolUse` remain blind spots and
cannot produce a Distill diagnostic.

Claude projection is explicit. `distill_read` and `distill_run` do not intercept
native Claude `Read` or `Bash`. MCP stdout contains JSON-RPC only, and
application failures use bounded `isError` tool results without raw source
bodies.

Sources:

- [Codex hooks](https://learn.chatgpt.com/docs/hooks.md)
- [MCP architecture](https://modelcontextprotocol.io/specification/2025-06-18/architecture)
