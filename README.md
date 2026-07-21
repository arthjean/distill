<p align="center">
  <img src="assets/distill-logo.png" alt="Distill" width="130" />
</p>

<h1 align="center">Distill</h1>

<p align="center">
  <b>Cut LLM token usage by up to 98% — before the context window ever fills.</b>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/distill-mcp"><img alt="npm version" src="https://img.shields.io/npm/v/distill-mcp?color=cb3837&logo=npm"></a>
  <a href="https://www.npmjs.com/package/distill-mcp"><img alt="npm downloads" src="https://img.shields.io/npm/dm/distill-mcp?color=cb3837"></a>
  <a href="https://github.com/arthjean/distill/actions/workflows/build.yml"><img alt="CI" src="https://github.com/arthjean/distill/actions/workflows/build.yml/badge.svg"></a>
  <a href="https://smithery.ai/server/@ArthurDEV44/distill-mcp"><img alt="Smithery" src="https://smithery.ai/badge/@ArthurDEV44/distill-mcp"></a>
  <a href="https://opensource.org/licenses/MIT"><img alt="License: MIT" src="https://img.shields.io/badge/License-MIT-yellow.svg"></a>
  <img alt="Node.js" src="https://img.shields.io/node/v/distill-mcp">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#the-3-tools">The 3 tools</a> ·
  <a href="#where-it-fits">Where it fits</a> ·
  <a href="https://distill-mcp.com">Docs</a> ·
  <a href="https://github.com/arthjean/distill/discussions">Discussions</a>
</p>

**Distill** is an open-source [MCP](https://modelcontextprotocol.io) server that compresses context *at the source*. Build output, logs, diffs, and whole-file reads get distilled down to the tokens that actually matter — so your agent reads more, spends less, and stays sharp deeper into a session. Three always-loaded tools, present from turn 1, replace dozens of individual calls. Works with Claude Code, Cursor, and Windsurf.

```ts
// Before Distill — 7 tool calls, ~2,500 tokens of overhead:
//   Read ×3  →  Grep  →  Read  →  Read  →  compress

// With Distill — 1 call, ~500 tokens:
code_execute(`
  const files = ["server.ts", "registry.ts", "executor.ts"];
  return ctx.compress.auto(
    files.map(f => ctx.code.skeleton(ctx.files.read(f), "typescript")).join("\n")
  );
`)
//                                              ▲  ~80% less overhead, same answer
```

## Why Distill?

Claude Code already compresses context *after* it enters the window (autocompact). Distill compresses *before* — catching large outputs at the source, so the expensive tokens never land in context at all.

| Problem | Distill tool | How | Savings |
|---------|-------------|-----|---------|
| Large build output, logs, diffs | `auto_optimize` | Auto-detects type, applies the best compressor | 40–95% |
| Reading an entire file for one function | `smart_file_read` | AST extraction across 7 languages | 50–90% |
| Chaining 5–10 tool calls | `code_execute` | A TypeScript SDK in a sandbox — one call | **up to 98%** |

## Where it fits

Distill doesn't replace your tools — it sits between their output and your context window and keeps the noise out.

| Approach | What you get | Where Distill fits |
|---|---|---|
| Raw tool calls (`Read` / `Grep` / `Bash`) | Full fidelity, full token cost | Compresses the output *before* it lands in context |
| Claude Code autocompact | Reclaims space *after* the window fills | Distill works *before* — those tokens never enter |
| Manual context trimming | Tedious and error-prone | Content-aware compressors do it automatically |
| A pile of single-purpose MCP tools | Broad surface, ~500 tokens of overhead *each* | 3 always-loaded tools, ~2,000 tokens total |

## Quick Start

```bash
# Run directly with npx
npx distill-mcp

# Or install globally
npm install -g distill-mcp

# Auto-configure your IDE (Claude Code, Cursor, Windsurf)
distill-mcp setup
```

### Add to Claude Code

```bash
claude mcp add distill -- npx distill-mcp
```

All 3 tools are available immediately — no discovery step, no loading modes.

## The 3 Tools

### `auto_optimize` — compress any content

Auto-detects content type and applies the optimal compression strategy.

| Strategy | Content type | Typical savings |
|----------|-------------|-----------------|
| `build` | Compiler errors (tsc, rustc, webpack) | 95% |
| `logs` | Server / test / build logs | 80–90% |
| `errors` | Repeated error lines | 70–90% |
| `diff` | Git diffs | 60–80% |
| `stacktrace` | Stack traces | 50–80% |
| `code` / `semantic` | Source code (TF-IDF) | 40–60% |
| `config` | JSON / YAML configs | 30–60% |
| `auto` | Auto-detect best strategy | varies |

```
auto_optimize content="<large build output>" strategy="auto"
```

### `smart_file_read` — AST-powered code reading

Read code with precision — extract exactly what you need.

**5 modes:**
- `skeleton` — function / class signatures only (depth 1–3)
- `extract` — pull a specific function, class, or interface by name
- `search` — find elements matching a query
- `full` — complete file structure overview
- `auto` — detect mode from params

**7 languages:** TypeScript, JavaScript, Python, Go, Rust, PHP, Swift

```
smart_file_read filePath="src/server.ts" mode="skeleton"
smart_file_read filePath="src/server.ts" mode="extract" target={"type":"function","name":"createServer"}
smart_file_read filePath="src/server.ts" mode="search" query="register"
```

### `code_execute` — TypeScript SDK in a sandbox

Write TypeScript instead of chaining tool calls. Access files, git, search, and compression through the `ctx.*` SDK.

```
code_execute code="return ctx.compress.auto(ctx.files.read('build.log'))"
```

**Batch multiple operations in one call:**

```typescript
// Read 3 files, extract key signatures, compress the result — 1 tool call instead of 7
const files = ["src/server.ts", "src/registry.ts", "src/executor.ts"];
const skeletons = files.map(f => ctx.code.skeleton(ctx.files.read(f), "typescript"));
return ctx.compress.auto(skeletons.join("\n---\n"));
```

**SDK API:**

```typescript
// File operations
ctx.files.read(path)         // Read file content
ctx.files.glob(pattern)      // Find files by pattern
ctx.files.exists(path)       // Check if file exists

// Code analysis
ctx.code.parse(content, lang)                  // Parse to structure
ctx.code.extract(content, lang, {type, name})  // Extract element
ctx.code.skeleton(content, lang)               // Signatures only

// Compression
ctx.compress.auto(content, hint?)    // Auto-detect and compress
ctx.compress.logs(logs)              // Log summarization
ctx.compress.diff(diff)              // Diff compression
ctx.compress.semantic(content, ratio?) // TF-IDF compression

// Git
ctx.git.diff(ref?)         // Get diff
ctx.git.log(limit?)        // Commit history
ctx.git.blame(file, line?) // Blame info
ctx.git.status()           // Working tree status
ctx.git.branch()           // Current branch info

// Search
ctx.search.grep(pattern, glob?)       // Search file contents
ctx.search.symbols(query, glob?)      // Find code symbols
ctx.search.files(pattern)             // Find files by pattern
ctx.search.references(symbol, glob?)  // Find symbol references

// Analysis
ctx.analyze.dependencies(file)           // File dependencies
ctx.analyze.callGraph(fn, file, depth?)  // Call graph
ctx.analyze.exports(file)               // Exported symbols
ctx.analyze.structure(dir, depth?)       // Directory structure

// Pipeline
ctx.pipeline(steps)                  // Run step array
ctx.pipeline.codebaseOverview(dir?)  // Quick overview
ctx.pipeline.findUsages(symbol)      // Find all usages
ctx.pipeline.analyzeDeps(file)       // Dependency analysis

// Utilities
ctx.utils.countTokens(text)      // Count tokens
ctx.utils.detectType(content)    // Detect content type
ctx.utils.detectLanguage(path)   // Detect language from path
```

## Token Overhead

Distill's 3 tools add minimal overhead to every API call:

| | Tool schemas | Description |
|--|-------------|-------------|
| **Distill** | ~2,000 tokens | 3 always-loaded tools |
| **Equivalent** | ~10,000+ tokens | 20+ individual tools doing the same |

All 3 tools use `_meta['anthropic/alwaysLoad']` — present from turn 1 with zero discovery friction.

## CLI Commands

```bash
distill-mcp setup          # Auto-configure detected IDEs
distill-mcp setup --claude # Configure Claude Code only
distill-mcp setup --cursor # Configure Cursor only
distill-mcp doctor         # Verify installation
distill-mcp serve          # Start the MCP server
distill-mcp analyze        # Analyze codebase token usage
distill-mcp --help         # Show help
```

## IDE Configuration

### Claude Code

After running `distill-mcp setup`, your config will include:

```json
{
  "mcpServers": {
    "distill": {
      "command": "npx",
      "args": ["distill-mcp", "serve"]
    }
  }
}
```

### Cursor / Windsurf

Configuration is added to the appropriate settings file automatically.

## Security

`code_execute` runs in a sandboxed environment with 7 security layers:

- **Static analysis** blocks `eval`, `require`, `import()`, `process`, `Reflect`, `Proxy`
- **QuickJS WASM isolation** — no `fetch`, no `fs`, no host access
- **File access** restricted to the working directory (symlinks resolved)
- **Sensitive files** blocked (`.env`, credentials, keys)
- **Git commands** allowlisted (no `push`, `fetch`, `clone`)
- **Memory limit** 128 MB · **timeout** 30 s · **output cap** 4,000 tokens (auto-compressed)

Found a vulnerability? See [SECURITY.md](.github/SECURITY.md) — please don't open a public issue.

## Development

```bash
bun install          # Install dependencies
bun run build        # Build all packages
bun run test         # Run tests
bun run dev          # Start dev mode (watch)
bun run check-types  # TypeScript type check
bun run lint         # ESLint
```

Built with Bun workspaces + Turborepo. The published package lives in `packages/mcp-server/`.

## Community

- **[GitHub Discussions](https://github.com/arthjean/distill/discussions)** — questions, ideas, feedback
- **[Issues](https://github.com/arthjean/distill/issues)** — bug reports
- **[Documentation](https://distill-mcp.com)** — full docs site

## Contributing

Contributions welcome — see [CONTRIBUTING.md](./CONTRIBUTING.md).

**Priority areas:**
- New language parsers for `smart_file_read` (Java, C#, Kotlin)
- SDK extensions for `code_execute`
- Documentation

If Distill saves you tokens, a ⭐ helps other people find it.

## License

[MIT](LICENSE) © Arthur Jean

---

<p align="center">
  <a href="https://www.npmjs.com/package/distill-mcp">npm</a> ·
  <a href="https://github.com/arthjean/distill">GitHub</a> ·
  <a href="https://distill-mcp.com">Documentation</a> ·
  <a href="https://github.com/arthjean/distill/discussions">Discussions</a>
</p>
