# AGENTS.md

Guidance for AI coding agents (OpenAI Codex CLI/App/IDE, and any other tool that
reads [AGENTS.md](https://agents.md)) working in this repository.

This file is **additive** — it does not duplicate `README.md` or
[`CLAUDE.md`](./CLAUDE.md). Claude Code reads `CLAUDE.md` (the long-form,
internals-heavy doc). Codex reads this file. When deep Claude-Code-specific
mechanics matter (autocompact formulas, MCP wire contract, PreCompact hook
semantics), `CLAUDE.md` is the authoritative source — this file refers out.

## Project overview

Distill is an open-source MCP server (`distill-mcp` on npm) that optimizes LLM
token usage through intelligent context compression. Bun workspaces +
Turborepo monorepo. Node `>= 20`. ESM only.

**Architecture — "3 Tools, Zero Friction"**: exactly 3 MCP tools, all with
`_meta['anthropic/alwaysLoad'] = true` so they stay present from turn 1:

- `auto_optimize` — universal content-aware compression
- `smart_file_read` — AST-powered reader (7 languages, 5 modes)
- `code_execute` — TypeScript SDK in QuickJS sandbox

## Repository layout

- `packages/mcp-server/` — **main package** (published as `distill-mcp`).
  Almost all development happens here.
- `packages/eslint-config/` — ESLint v9 flat configs (`base.js`, `next.js`).
- `packages/typescript-config/` — TS presets (`base.json`, `nextjs.json`).

The landing + fumadocs docs site (formerly `apps/web/`) moved to the external
private repo `arthjean/distill-web`, deployed to distill-mcp.com.

## Environment setup

```bash
bun install                    # install all workspace deps
```

Node `>= 20` required. `package.json` `"type": "module"` — all local imports
MUST carry a `.js` extension even for `.ts` sources.

## Build

```bash
bun run build                  # build all packages (turbo)
bun run dev                    # dev mode with watch (turbo)
bun run dev:mcp                # mcp-server only
bun run dev:web                # web app only (uses --turbopack)
```

## Test

```bash
# From packages/mcp-server/
cd packages/mcp-server
bun run test                   # vitest run (30s timeout for Tree-sitter WASM init)
bun run test:watch             # watch mode
bun run test:coverage          # V8 coverage report

# Run a single test file
npx vitest run src/path/to/file.test.ts
```

Coverage floors (vitest.config.ts — CI fails below): lines 70%, branches 56%,
functions 70%, statements 69%. Do not let new code regress these.

## Lint / format / typecheck

```bash
bun run lint                   # ESLint all packages
bun run format                 # Prettier
bun run check-types            # tsc --noEmit all packages
```

## Code conventions

- **Package manager: `bun` only.** Never `npm`, `pnpm`, `yarn`. Never `npx` — use
  `bunx`. This is enforced by a global pre-tool hook; attempting `npm`/`npx` in
  a JS/TS project will be blocked.
- **ES Modules** (`"type": "module"`). All local imports MUST use `.js`
  extensions (`import { foo } from "./bar.js"` even when `bar.ts` is the source).
- **Error handling**: `neverthrow` `Result<T, E>` with discriminated-union errors
  using a `code` literal discriminant. Factories use
  `as const satisfies FactoryInterface`. No bare `throw` in public APIs.
- **Tests**: co-located as `*.test.ts`. Vitest with `globals: true`. 30s
  timeout is set for Tree-sitter WASM init — do not lower it.
- **Commit format**: Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`,
  `test:`, `chore:`).
- **CLI**: manual `process.argv` parsing in `packages/mcp-server/bin/cli.js`.
  Subcommands: `serve`, `setup`, `doctor`, `analyze`.

## PR rules

- **PRs target `dev`**, not `main`. Releases merge `dev` → `main`.
- CI runs 5 parallel jobs: lint, typecheck, test (mcp-server only, with
  coverage + Tree-sitter WASM retry), build, knip. All must pass.
- Before pushing, run locally: `bun run check-types && bun run lint` plus the
  relevant tests for touched packages.

## Critical do-not rules

These are non-obvious gotchas that WILL cause broken behavior in production if
ignored. They are the highest-priority rules in this file.

1. **Never `console.log` in the MCP server.** Transport is stdio — stdout is
   the MCP protocol channel. All diagnostic output MUST use `console.error`.
2. **Do not set `structuredContent` on the MCP wire response** in
   `packages/mcp-server/src/server.ts`. Claude Code stashes it in `mcpMeta`
   which is explicitly excluded from the Anthropic API blocks. Emitting it is
   pure bandwidth cost. Internal `structuredContent` on `ToolResult` is fine
   (kept for non-Claude-Code MCP clients and tests).
3. **Do not emit `_meta['anthropic/searchHint']`.** For tools with
   `alwaysLoad: true`, the hint is unreachable by construction. Only
   `alwaysLoad` is emitted in `_meta`.
4. **Do not upgrade `web-tree-sitter` past `0.22.6`** or `@sebastianwessel/quickjs`
   past `3.0.0` without review. Both are pinned exact versions. The WASM
   sandbox is the primary security boundary.
5. **Do not introduce a 4th MCP tool.** The design is exactly 3 tools — each
   new tool adds ~500 tokens per API call and every description is truncated
   at 2048 chars by Claude Code.
6. **Do not change `readOnlyHint` annotations without updating behavior.**
   `auto_optimize` and `smart_file_read` declare `readOnlyHint: true`;
   `code_execute` declares `readOnlyHint: false, destructiveHint: true`.
   If a tool gains a write side-effect, update the annotation in the same
   change — Claude Code dispatches read-only tools in parallel.
7. **TS/JS parsing uses the TypeScript Compiler API, NOT Tree-sitter.** Only
   the 5 other languages (Python, Go, Rust, PHP, Swift) use Tree-sitter WASM.
   First sync parse call on a new language returns an empty `FileStructure`
   and warms the parser — this is by design.
8. **Do not commit secrets.** `.env*` files are gitignored. The repo is public.

## Definition of done

A change is complete when ALL of the following pass locally for touched
packages:

```bash
bun run check-types
bun run lint
cd packages/mcp-server && bun run test      # if mcp-server touched
```

Additionally:
- New behavior has a test co-located as `*.test.ts`.
- Coverage floors in `packages/mcp-server/vitest.config.ts` have not regressed.
- No new `console.log` anywhere in `packages/mcp-server/src/`.
- Conventional Commit message with a clear scope.

## Human-required actions

Do NOT perform these without explicit user confirmation in the session:

- **Publishing to npm** (`distill-mcp`) — always user-initiated.
- **Merging `dev` → `main`** — release gate.
- **Force-push, `git reset --hard`, deleting branches** — destructive.
- **Version bumps** + `CHANGELOG.md` edits — part of the release process.
- **Upgrading pinned dependencies** (`web-tree-sitter`, `@sebastianwessel/quickjs`).
- **Editing `.github/workflows/`** — CI changes need review.

## MCP / agent configuration

This project IS an MCP server. When Codex runs commands in this repo:

- The published server is `distill-mcp` (stdio transport).
- Local dev server: `bun run dev:mcp` (runs `packages/mcp-server` in watch mode).
- The 3 tools are registered directly in `packages/mcp-server/src/server.ts` —
  no dynamic loading, no discovery mechanism.
- Subcommand `distill-mcp setup` installs config for Claude Code / Cursor /
  Windsurf / Continue.
- Subcommand `distill-mcp doctor` diagnoses installation issues.

## Deeper references

When you need the full Claude Code internals — autocompact formula, MCP
result-persistence threshold, PreCompact hook contract, compression marker
`[DISTILL:COMPRESSED]` semantics, per-tool thresholds, the 11-row verified
citations table into `claude-code/<path>:<line>` — read
[`CLAUDE.md`](./CLAUDE.md). That file is the source of truth for Claude-Code-
specific mechanics and is not duplicated here.
