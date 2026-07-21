# Distill

> Extract the essence. Compress the context. Save tokens.

**Distill** is an open-source MCP server that optimizes LLM token usage through intelligent context compression. Works with Claude Code, Cursor, and Windsurf.

[![npm version](https://img.shields.io/npm/v/distill-mcp.svg)](https://www.npmjs.com/package/distill-mcp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## Why Distill?

| Problem | Distill Solution | Savings |
|---------|------------------|---------|
| Large build outputs | Auto-compress errors | 80-95% |
| Reading entire files | AST-based extraction | 50-70% |
| Multiple tool calls | TypeScript SDK execution | **98%** |
| Verbose logs | Smart summarization | 80-90% |

## Quick Start

```bash
# Run directly with npx
npx distill-mcp

# Or install globally
npm install -g distill-mcp

# Configure your IDE
distill-mcp setup
```

### Add to Claude Code

```bash
claude mcp add distill -- npx distill-mcp
```

## Features

- **Smart File Reading** - Extract functions, classes, or signatures without loading entire files
- **Auto Compression** - Detects content type and applies optimal compression
- **Code Execution SDK** - Write TypeScript instead of chaining tool calls
- **Lazy Loading** - Only loads tools when needed (85% token overhead reduction)
- **7 Languages** - TypeScript, JavaScript, Python, Go, Rust, PHP, Swift

## MCP Tools

### Core Tools (Always Loaded)

| Tool | Purpose | Savings |
|------|---------|---------|
| `auto_optimize` | Auto-detect and compress content | 40-95% |
| `smart_file_read` | Read code with AST extraction | 50-70% |
| `code_execute` | Execute TypeScript with SDK | **98%** |
| `discover_tools` | Browse/load additional tools | - |

### On-Demand Tools

| Tool | Purpose | Savings |
|------|---------|---------|
| `semantic_compress` | TF-IDF based compression | 40-60% |
| `summarize_logs` | Summarize server/test/build logs | 80-90% |
| `analyze_build_output` | Parse build errors | 95%+ |
| `deduplicate_errors` | Group repeated errors | 80-95% |
| `diff_compress` | Compress git diffs | 50-80% |
| `context_budget` | Pre-flight token estimation | - |
| `session_stats` | Usage analytics | - |

## Usage Examples

### Smart File Reading

```bash
# Get file structure overview
mcp__distill__smart_file_read filePath="src/server.ts"

# Extract specific function
mcp__distill__smart_file_read filePath="src/server.ts" target={"type":"function","name":"createServer"}

# Get skeleton (signatures only)
mcp__distill__smart_file_read filePath="src/server.ts" skeleton=true
```

### Compress Build Output

```bash
# After a failed build, compress the output
mcp__distill__auto_optimize content="<paste npm/tsc/webpack output>"
```

### Code Execution SDK

The `code_execute` tool provides **98% token savings** by letting LLMs write TypeScript:

```bash
mcp__distill__code_execute code="return ctx.compress.auto(ctx.files.read('logs.txt'))"
```

**SDK API:**

```typescript
// File operations
ctx.files.read(path)
ctx.files.glob(pattern)
ctx.files.exists(path)

// Code analysis
ctx.code.skeleton(content, lang)
ctx.code.extract(content, lang, {type, name})
ctx.code.parse(content, lang)

// Compression
ctx.compress.auto(content, hint?)
ctx.compress.logs(logs)
ctx.compress.diff(diff)
ctx.compress.semantic(content, ratio?)

// Git operations
ctx.git.diff(ref?)
ctx.git.log(limit?)
ctx.git.blame(file, line?)

// Search
ctx.search.grep(pattern, glob?)
ctx.search.symbols(query)

// Analysis
ctx.analyze.dependencies(file)
ctx.analyze.callGraph(fn)
```

### Discover Tools

```bash
# Browse available tools (metadata only)
mcp__distill__discover_tools category="compress"

# Load tools when needed
mcp__distill__discover_tools category="compress" load=true

# TOON format for compact output
mcp__distill__discover_tools format="toon"
```

## CLI Commands

```bash
distill-mcp setup          # Auto-configure detected IDEs
distill-mcp setup --claude # Configure Claude Code only
distill-mcp setup --cursor # Configure Cursor only
distill-mcp doctor         # Verify installation
distill-mcp serve          # Start MCP server
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

Configuration is automatically added to the appropriate settings file.

## Token Overhead

Distill uses **lazy loading** to minimize overhead:

| Mode | Tokens | Description |
|------|--------|-------------|
| Core only | 264 | Default (4 tools) |
| All tools | 1,108 | Full suite (21 tools) |
| **Savings** | **76%** | Lazy vs eager loading |

## Security

Code execution runs in a sandboxed environment:
- Blocked: `eval`, `require`, `import()`, `process`, `global`
- File access restricted to working directory
- Sensitive files blocked (`.env`, credentials, keys)
- Memory limit: 128MB, Timeout: 30s

## Development

```bash
# Install dependencies
bun install

# Run tests
bun run test

# Build
bun run build

# Start dev server
bun run dev
```

## Contributing

Contributions welcome! See [CONTRIBUTING.md](https://github.com/arthjean/distill/blob/main/CONTRIBUTING.md) for guidelines.

**Priority areas:**
- New language parsers (Java, C#, Kotlin)
- SDK extensions
- Documentation

## License

MIT

---

**[npm](https://www.npmjs.com/package/distill-mcp)** · **[GitHub](https://github.com/arthjean/distill)** · **[Documentation](https://github.com/arthjean/distill/tree/main/docs)**
