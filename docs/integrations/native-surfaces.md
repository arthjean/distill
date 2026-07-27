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
that backup with `--restore`. Codex setup rejects `--root`; Claude setup rejects
`--mode` and duplicate root IDs. These target-specific options fail before
configuration mutation.

Codex replacement uses documented `PostToolUse` blocking feedback because
`updatedMCPToolOutput` is parsed but unsupported. The adapter caps feedback at
2,250 tokens against Codex's approximate 2,500-token model-visible hook-output
limit. The versioned matrix is
[`codex-hook-conformance-v1.json`](codex-hook-conformance-v1.json). Hosted tools
and specialized paths that do not emit `PostToolUse` remain blind spots and
cannot produce a Distill diagnostic. One executable conformance test binds the
adapter input version, cap, supported modes, setup matcher, timeout, status
message, and generated command arguments to that matrix.

Claude projection is explicit. `distill_read` and `distill_run` do not intercept
native Claude `Read` or `Bash`. MCP stdout contains JSON-RPC only, and
application failures use bounded `isError` tool results without raw source
bodies. An oversized 1 MiB MCP frame produces one protocol error, drains only
that frame when needed, and resumes at the next newline. The published
`distill_run` schema shares the engine's 4,096-argument, 1,048,576-byte
executable-plus-argv, and 100-through-300,000-ms timeout limits. Its `argv`
field is required by both the published schema and runtime decoder, including
when the literal argument vector is empty.

CLI JSON failure framing is selected only by a parsed Distill `--json` option.
Values after the `run -- EXECUTABLE` delimiter remain literal child argv and
cannot select Distill output mode.

CLI status uses `distill.status/v2` and includes `lineage_bytes` plus
`max_lineage_bytes`. Garbage collection uses `distill.gc/v2` and separately
reports reclaimed source and lineage bytes. Artifact trace remains complete and
ordered within the 1 MiB per-artifact receipt limit.

No v1 support is claimed for Cursor, Windsurf, Continue, generic MCP clients,
Windows, macOS x86_64, Linux arm64, or Linux musl. The complete migration and
unsupported-surface matrix is
[`mcp-first-to-native.md`](../migration/mcp-first-to-native.md). The adapter
boundaries and direct-native-asset decision are documented in
[`native-context-projection-engine.md`](../architecture/native-context-projection-engine.md).

Sources:

- [Codex hooks](https://learn.chatgpt.com/docs/hooks.md)
- [MCP architecture](https://modelcontextprotocol.io/specification/2025-06-18/architecture)
