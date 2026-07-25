# distill-mcp (legacy)

`distill-mcp` is the frozen MCP-first version of Distill. It exposes three tools
for explicit context compression, structured file reading, and sandboxed
TypeScript execution.

This README documents the published `distill-mcp` package only. The active
product direction is the native Rust context projection engine described in the
[repository README](../../README.md). The native replacement has not been
published, and this package remains available until its separately approved
retirement.

## What this package does

The server registers exactly three always-loaded MCP tools:

| Tool | Behavior |
| --- | --- |
| `auto_optimize` | Compress explicitly supplied build output, logs, diffs, errors, or text with a content-aware strategy |
| `smart_file_read` | Read files through exact, line-range, search, skeleton, structure, or symbol-extraction modes |
| `code_execute` | Run TypeScript against the `ctx.*` SDK inside the pinned QuickJS sandbox |

`smart_file_read` supports TypeScript, JavaScript, Python, Go, Rust, PHP, and
Swift. TypeScript and JavaScript use the TypeScript Compiler API; the other
languages use Tree-sitter WASM.

There is no lazy tool catalog, `discover_tools`, or set of on-demand MCP tools
in the current package.

## Architectural limits

An MCP server does not own the host's agent loop or context aggregation.
Consequently:

- native host tool output reaches the conversation before `auto_optimize` can
  process it;
- the legacy Claude hooks suggest Distill tools but do not replace native
  `Read` or `Bash` results;
- original-result recovery is process-scoped and does not survive restart;
- Cursor, Windsurf, Antigravity, and generic MCP configuration paths are legacy
  integrations, not qualified vNext support surfaces.

These limits motivated the native projection engine.

## Install and run the legacy package

Requirements: Node.js 20 or later.

Run without a global installation:

```bash
bunx distill-mcp --help
bunx distill-mcp setup
```

Or install the CLI:

```bash
bun add --global distill-mcp
distill-mcp setup
distill-mcp doctor
```

Start the stdio MCP server directly:

```bash
distill-mcp serve
```

Example MCP configuration:

```json
{
  "mcpServers": {
    "distill": {
      "command": "bunx",
      "args": ["distill-mcp", "serve"]
    }
  }
}
```

## CLI

| Command | Purpose |
| --- | --- |
| `distill-mcp serve` | Start the stdio MCP server |
| `distill-mcp setup` | Configure a detected legacy MCP client |
| `distill-mcp doctor` | Diagnose installation and configuration |
| `distill-mcp analyze` | Report file and token-size hotspots |

Run `distill-mcp --help` for setup targets, hook options, analysis flags, and
verbose server diagnostics.

## Migration to native Distill

The native `distill` binary changes the product contract from post-hoc MCP
compression to bounded, recoverable context projection:

| Legacy use case | Native disposition |
| --- | --- |
| `auto_optimize` on supported Codex local-tool output | Automatic projection through the versioned `PostToolUse` adapter |
| `auto_optimize` on manually supplied bytes | `distill project --budget N` |
| Claude file or process acquisition | Explicit `distill_read` and `distill_run` MCP tools |
| Full-file bounded reading | `distill read` or `distill_read` |
| AST skeleton, search, and symbol extraction | Deferred from native v1 |
| `code_execute` operation batching | Use the host agent's native tools |
| Fixed local command execution | `distill run` or `distill_run` |
| Process-scoped original recovery | Durable `artifact get`, `artifact trace`, and `status` commands |

Native Distill commits source bytes before returning an omitting projection,
applies an explicit byte or token budget, and returns an artifact reference plus
a receipt describing retained and omitted spans.

The complete compatibility and retirement decisions are recorded in the
[migration guide](../../docs/migration/mcp-first-to-native.md).

## Current migration status

The native engine, CLI, Codex adapter, and Claude adapter are implemented.
macOS arm64 is qualified on the current native tree. Linux x86_64 packaging is
prepared but still requires same-tree qualification. No native asset has been
published.

The TypeScript MCP package remains frozen as migration evidence. Its deletion,
any version change, and any publication remain separate maintainer-approved
actions.

## Development

From the repository root:

```bash
bun install
bun run check-types
bun run lint
cd packages/mcp-server
bun run test
```

The package is ESM-only. Local TypeScript imports use `.js` extensions, and
server diagnostics must go to stderr because stdout is the MCP protocol
channel.

## License

[MIT](../../LICENSE)
