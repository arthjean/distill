# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Distill is an open-source MCP server (`distill-mcp` on npm) that optimizes LLM token usage through intelligent context compression. Designed primarily for Claude Code. Bun workspaces + Turborepo monorepo.

**Architecture: "3 Tools, Zero Friction"** — 3 always-loaded tools with `_meta['anthropic/alwaysLoad'] = true`:
- `auto_optimize` — Universal content-aware compression (build output, logs, diffs, code, stacktraces)
- `smart_file_read` — AST-powered code reading for 7 languages. 5 modes: `auto`, `full`, `skeleton` (signatures + depth 1-3), `extract` (by name), `search` (by query)
- `code_execute` — TypeScript SDK in QuickJS sandbox (batch 5-10 operations in 1 call)

## Monorepo Structure

- `packages/mcp-server/` — **Main package** (published as `distill-mcp`). Almost all development happens here. `SupportedLanguage` / `ContentType` live in `src/ast/types.ts`.
- `packages/eslint-config/` — ESLint v9 flat configs: `base.js`, `next.js`
- `packages/typescript-config/` — TypeScript presets: `base.json`, `nextjs.json`

The landing page + fumadocs docs site (formerly `apps/web/`) was extracted to the external private repo **`arthjean/distill-web`** (deployed to distill-mcp.com via the `distill-mcp` Vercel project). It is no longer part of this monorepo. The `eslint-config` `next.js` export and `typescript-config` `nextjs.json` preset remain here but are now only consumed by that external repo's history — they can be pruned in a later cleanup.

## Commands

```bash
bun install                    # Install dependencies
bun run build                  # Build all packages (turbo)
bun run dev                    # Dev mode with watch (turbo)
bun run dev:mcp                # Dev MCP server only
bun run dev:web                # Dev web app only (uses --turbopack)
bun run lint                   # ESLint all packages
bun run format                 # Prettier format
bun run check-types            # TypeScript type check all packages

# MCP server tests (from packages/mcp-server/)
cd packages/mcp-server
bun run test                   # Run tests (vitest)
bun run test:watch             # Watch mode
bun run test:coverage          # V8 coverage report
```

Run a single test: `cd packages/mcp-server && npx vitest run src/path/to/file.test.ts`

### Coverage thresholds (mcp-server)

`vitest.config.ts` enforces minimums on `src/**/*.ts` (excluding tests, `*.d.ts`, and `types.ts`). CI fails when any category drops below floor:

| Category   | Floor (v0.9.2 ratchet)      | Baseline (2026-04-21) | Target |
|------------|-----------------------------|-----------------------|--------|
| Lines      | 70%                         | 71.80%                | 75%    |
| Branches   | 56%                         | 57.58%                | 70%    |
| Functions  | 70%                         | 71.93%                | 75%    |
| Statements | 69%                         | 70.99%                | 75%    |

Floors were raised by v0.9.2 US-011 from the initial v0.9.1 floors (baseline − 2pts) to the current baseline − 1pt buffer. Raise toward v1.0 targets as new tests land.

## Architecture Gotchas

These are non-obvious behaviors that WILL cause mistakes if not understood.

### Claude Code Integration (CRITICAL for Distill's design)

- **Claude Code defers ALL MCP tools by default** via its ToolSearch mechanism. Use `_meta['anthropic/alwaysLoad'] = true` on tools that must be present from turn 1.
- **Tool descriptions are truncated at 2048 chars** by Claude Code. Front-load the most important info.
- **Each tool adds ~500 tokens overhead** per API call. Fewer tools = less overhead. This is why we have only 3.
- **MCP tool results are persisted to disk when they exceed ~25K tokens** by Claude Code, with only a 2KB preview shown to the model. The threshold is `DEFAULT_MAX_MCP_OUTPUT_TOKENS = 25_000` at `claude-code/utils/mcpValidation.ts:16`. A heuristic gate (`roughTokenCountEstimation = length/4`) at `claude-code/utils/mcpValidation.ts:151-163` short-circuits when content is ≤ `25_000 * MCP_TOKEN_COUNT_THRESHOLD_FACTOR (0.5) = 12_500` estimated tokens (≈ 50K chars), skipping a real API token count and skipping truncation. Note: the 50K-char figure that previously appeared here refers to `DEFAULT_MAX_RESULT_SIZE_CHARS` in `claude-code/constants/toolLimits.ts:13`, which applies to **built-in** tools only — MCP tools take the separate `mcpValidation.ts` path.
- **Autocompact formula (per `claude-code/services/compact/autoCompact.ts:33-48, 62, 72-76`)**: `reserved = min(getMaxOutputTokensForModel(model), MAX_OUTPUT_TOKENS_FOR_SUMMARY)` where `MAX_OUTPUT_TOKENS_FOR_SUMMARY = 20_000` (`autoCompact.ts:30`); `effectiveContextWindow = contextWindow - reserved`; `autoCompactThreshold = effectiveContextWindow - AUTOCOMPACT_BUFFER_TOKENS` where `AUTOCOMPACT_BUFFER_TOKENS = 13_000` (`autoCompact.ts:62`). Worked examples: Haiku (200K context, native `max_output = 4_096`) → `reserved = min(4_096, 20_000) = 4_096` → trigger = `(200_000 − 4_096) − 13_000 = 182_904 tokens (≈ 91.5%)` (notably higher than the Sonnet/Opus trigger — Haiku users have more headroom before autocompact); Sonnet/Opus 200K (`max_output ≥ 20_000`) → `reserved = 20_000` → trigger = `(200K − 20K) − 13K = 167K tokens (83.5%)`; 1M-context model → `reserved = 20_000` → trigger = `(1M − 20K) − 13K = 967K tokens`. Distill compresses BEFORE content enters context; Claude Code compresses AFTER.
- **Subagents get ALL MCP tools** automatically — Distill's 3 tools are available in every subagent session.
- **`_meta['anthropic/searchHint']` is not emitted by Distill.** It is a scoring-only signal inside Claude Code's ToolSearch (see appendix row #2) — never rendered in the deferred-tools prompt. For `alwaysLoad: true` tools the hint is unreachable by construction, so emitting it would waste bytes without enabling any discovery behavior.
- **MCP tools are sorted alphabetically** after built-in tools in the system prompt. `distill` starts with 'd'.

### MCP Server (`src/server.ts`)

- **Transport is stdio.** All diagnostic output MUST use `console.error`, never `console.log` — stdout is the MCP protocol channel.
- **3 tools registered directly** — no dynamic loading, no catalog, no discovery mechanism.
- **Server `instructions` field** is set on the MCP Server constructor — a static 4-line string explaining when to use each tool. No dynamic data (breaks prompt caching).
- **Only `anthropic/alwaysLoad` is emitted in `_meta`.** `anthropic/searchHint` is intentionally not set (see appendix row #2 for the full chain via `claude-code/tools/ToolSearchTool/prompt.ts:112-116`).
- **`maxResultSizeChars` is intentionally NOT set.** Claude Code reads this from the top-level Tool object (not `_meta`), and in any case the MCP SDK strips unknown top-level Tool properties via Zod — so there is no valid place to put it. Output size is enforced in-tool via the 45K-char budget cap (see `auto_optimize` and `smart_file_read`). The cap is sized to stay under the MCP heuristic gate (`length/4` ≤ 12,500 tokens, per `claude-code/utils/mcpValidation.ts:151-163`), which short-circuits truncation for content well below the 25K-token hard ceiling at `claude-code/utils/mcpValidation.ts:16`. The built-in-tool constant `DEFAULT_MAX_RESULT_SIZE_CHARS = 50_000` at `claude-code/constants/toolLimits.ts:13` is a different code path and does not gate MCP tools.
- **`outputSchema` is intentionally omitted** from the `tools/list` response. Claude Code's MCP client silently ignores `outputSchema` during the `tools/list` → internal `Tool` mapping (`claude-code/services/mcp/client.ts:1754-1813` reads only `description`, `inputSchema`, `_meta.*`, and `annotations.*`); tools carrying it are neither dropped nor rejected — the field is simply not copied. Distill's tool definitions internally retain `outputSchema` for documentation.
- **`structuredContent` is NOT emitted on the wire.** All 3 tools internally produce `structuredContent` on their `ToolResult` (kept in `packages/mcp-server/src/tools/registry.ts:27` for test assertions and non-Claude-Code MCP clients that read it via the SDK surface), but the CallTool handler in `src/server.ts` returns only `{ content, isError }`. Rationale: Claude Code stashes `structuredContent` in `mcpMeta` and explicitly excludes `mcpMeta` from Anthropic-API blocks (see appendix row #6) — so emitting it on the wire is pure bandwidth cost for zero model-side value.
- **Tool change notification** (`notifications/tools/list_changed`) is wrapped in try/catch because it may fire before the stdio transport is connected.
- **Tool `annotations` must be re-evaluated whenever a tool's behavior changes.** The current mapping — `auto_optimize` and `smart_file_read` declare `readOnlyHint: true`, `code_execute` declares `readOnlyHint: false, destructiveHint: true` — is correct today because the first two are pure read/compute and `code_execute` can mutate files via `ctx.files` and git state. If a contributor adds a write side-effect to `auto_optimize`/`smart_file_read` (or removes mutation from `code_execute`), the annotations block **must** be updated in the same change: Claude Code maps `annotations.readOnlyHint` to `isConcurrencySafe()` / `isReadOnly()` (see appendix row #10), so a stale `readOnlyHint: true` on a tool that now writes would let Claude Code dispatch it in parallel with other reads, creating concurrency hazards on the filesystem.

### Compression Marker Contract (`[DISTILL:COMPRESSED]`)

Opt-in wire envelope that lets Claude Code's PreCompact hook preserve Distill-compressed regions verbatim during autocompact. Helper: `packages/mcp-server/src/utils/distill-marker.ts`.

- **Format:** `[DISTILL:COMPRESSED ratio=X.XX method=<name>]\n<payload>\n[/DISTILL:COMPRESSED]`. `ratio` is `compressed_size / original_size` clamped to `[0, 1]` with exactly 2 decimals. `<name>` is the compressor or mode that produced the payload (`auto`, `logs`, `diff`, `semantic`, `skeleton`, `extract`, `search`, `build+recompressed`, etc. — sanitised to `[A-Za-z0-9+_.-]` by the helper).
- **Opt-in.** Enabled when `process.env.DISTILL_COMPRESSED_MARKERS` is `1` / `true` / `yes` (case-insensitive). Default off — existing v0.9.x consumers see unchanged output. This satisfies hard constraint C2 of the v0.10 PRD.
- **Per-tool thresholds** (must all be met for a wrap, otherwise the raw text returns unwrapped — no half-envelopes):
  - `auto_optimize`: savings ≥ 30% (`compressionRatio ≤ 0.7`). Short-input pass-through and the error path never wrap.
  - `smart_file_read`: `text.length < 0.5 * originalFileLength` AND mode ∈ `{skeleton, extract, search}`. `full` and `lines` modes never wrap. Wrapping is applied in both the `cacheAndReturn` path and the cache-hit path so a cached result still emits with a fresh envelope.
  - `code_execute` / `ctx.compress.*`: wrapping lives inside the `compressAuto`, `compressLogs`, `compressDiff`, `compressSemantic` helpers in `packages/mcp-server/src/sandbox/sdk/compress.ts`. When enabled and savings ≥ 30%, the returned `CompressResult.compressed` (or `LogSummary.summary`) is wrapped; sandbox code that logs or returns the string carries the envelope to the outer tool output.
- **Collision escape.** If the payload already contains the literal `[DISTILL:COMPRESSED` or `[/DISTILL:COMPRESSED]` substring, the wrapper uses the fallback tokens `[DISTILL-USER-TEXT:COMPRESSED … ][/DISTILL-USER-TEXT:COMPRESSED]` so a consumer can distinguish user-authored text from the marker boundary.
- **Why it matters.** The marker is the channel the PreCompact hook (shipped by US-009 / US-010) references when it instructs the compact-summary LLM to preserve the region. No MCP protocol field survives autocompact (see appendix row #6 — `mcpMeta` is excluded from the Anthropic API blocks), so LLM-visible text is the only durable contract, and the envelope is that text's anchor.
- **PreCompact hook script.** `packages/mcp-server/scripts/precompact-hook.sh` is the shipped POSIX shell hook (US-009). It emits a plain-text instruction on stdout that Claude Code merges into `newCustomInstructions` (per `claude-code/utils/hooks.ts:3991-4024`); the instruction tells the compact-summary LLM to preserve any `[DISTILL:COMPRESSED …]` region verbatim. The script exits 0 on every input shape (including malformed JSON and unexpected events) so it can never block compaction. `--help` prints the full marker contract. Installation wiring into `~/.claude/settings.json` lands in US-010 (`distill-mcp setup --install-precompact-hook`).

### AST Parsing (`src/ast/`)

- **TypeScript/JavaScript use the TS Compiler API** (`import ts from "typescript"`), NOT Tree-sitter. All other 5 languages (Python, Go, Rust, PHP, Swift) use Tree-sitter WASM.
- **Tree-sitter sync/async split:** Sync `parseFoo()` returns empty `FileStructure` if WASM not yet initialized, and fires `initParser().catch(()=>{})` as a warm-up side-effect. Async `parseFooAsync()` awaits init. **First sync call to a new language parser returns empty results** — this is by design.
- `web-tree-sitter` is **pinned** at `0.22.6` (not caret range). Do not upgrade without testing all 5 Tree-sitter grammars.
- **Quick scan** (`quick-scan.ts`) is a regex fast path, ~90% faster. Trade-off: no `endLine`, no signatures, no documentation. Enabled via `parseFile(content, lang, 'quick')`.
- **Language mapping conventions:** Rust struct/enum -> `classes`, trait -> `interfaces`. Swift struct -> `classes`, protocol -> `interfaces`. PHP trait -> `classes`, enum -> `types`.

### Compression (`src/compressors/`)

- **`detectContentType()` does NOT detect `build` or `diff`.** Those are detected in `tools/auto-optimize.ts` via `isBuildOutput()` / `isDiffOutput()` before falling back to the generic compressor dispatch.
- `semanticCompressor` and `diffCompressor` are exported but **NOT in the default compressor priority array**. They are invoked via direct import in `auto_optimize`, not via `compressContent()`.
- **TF-IDF scoring** uses a 3-signal weighted model: TF-IDF (0.4) + position U-curve (0.3) + keyword boosts (0.3).
- Token counting uses `js-tiktoken` with `gpt-4` encoding (cl100k_base). Fallback: `Math.ceil(length / 4)`.

### Summarizers (`src/summarizers/`)

- **4 implementations**: `serverLogsSummarizer`, `testLogsSummarizer`, `buildLogsSummarizer`, `genericSummarizer`. The first three match content-type-specific shapes; `genericSummarizer` is the fallback and is also reached directly from `auto_optimize` + `sandbox/sdk/compress` when a specialized summarizer does not `canSummarize()` the input.
- **`genericSummarizer` internally composes three modules** — `scoring.ts` (BM25 + multi-factor importance), `clustering.ts` (semantic grouping), `pattern-extraction.ts` (template mining). These are production code, not optional plug-ins. They are re-exported from the `summarizers/` barrel for test access and sandbox SDK composition; they were mis-labeled as "advanced 2026 enhancements" in v0.9.1 docs and formally accepted as load-bearing in v0.9.2 US-010.

### Sandbox (`src/sandbox/`)

- **7 security layers:** (1) static regex code analysis, (2) QuickJS WASM isolation (no fetch/fs), (3) path validation + symlink resolution, (4) git command allowlist, (5) error message sanitization (strips host paths), (6) `safe-walk` directory traversal guards, (7) output token cap with auto-compression.
- **Blocked in user code:** `eval`, `require`, `import()`, `process`, `global`, `globalThis`, `Reflect`, `Proxy`, `setTimeout`, `__proto__`, `../..`.
- **`ctx.pipe` is available in both QuickJS and legacy modes.** The guest-side implementation runs callbacks locally in QuickJS and delegates I/O steps to host bridge functions. `ctx.pipeline` (step-array API) also works in both modes.
- **Dual API pattern:** `createFooAPI()` (returns `Result<T, E>`) and `createFooAPILegacy()` (throws). The QuickJS bridge uses the legacy throwing versions because `Result` objects cannot cross the WASM boundary.
- **Git SDK uses `execFileSync`** (not `execSync`) to bypass shell interpretation — `%(refname:short)` format specifiers pass through safely. **`LC_ALL=C`** is set to force English error messages regardless of system locale.
- Default limits: 5s timeout (30s max), 128MB memory, 4000 output tokens (auto-compressed if exceeded).
- `@sebastianwessel/quickjs` is **pinned** at `3.0.0` exact (not caret range). The WASM sandbox is the primary security boundary; upgrades are reviewed manually before the pin moves.

### Tool Registry (`src/tools/registry.ts`)

- Verbose-mode logging is **inlined** in `ToolRegistry.execute` as two `if (verbose)` blocks (before-call and after-call). No middleware abstraction — previously lived in `src/middleware/`, removed in v0.9.1 per PRD US-013.
- The after-call log fires on both happy and catch paths, so thrown handler errors are still surfaced to stderr when `verbose: true`.
- Pass verbose through `createToolRegistry(verbose)` or `createServer({ verbose: true })`.

## Key Patterns

- **Error handling:** `neverthrow` (`Result<T, E>`) with discriminated union errors using `code` literal discriminant. Factories use `as const satisfies FactoryInterface`.
- **ES Modules:** `"type": "module"` — all local imports MUST use `.js` extensions.
- **Test files:** Co-located as `*.test.ts`. Vitest with `globals: true`. **30s test timeout** for Tree-sitter WASM init.
- **Lazy singletons:** WASM parsers and HuggingFace embeddings use `let promise: Promise | null` pattern with retry-on-failure.
- **Conventional Commits:** `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`
- **Branch strategy:** PRs target `dev`, releases merge to `main`
- **Node requirement:** `>= 20`
- **CLI:** Manual `process.argv` parsing in `bin/cli.js`. Commands: `serve`, `setup`, `doctor`, `analyze`.
- **CI:** 5 parallel jobs — lint, typecheck, test (mcp-server only, runs with coverage + Tree-sitter WASM retry), build, knip. All on `ubuntu-latest` with Bun.

## Anti-Friction Rules (claude-doctor)

Règles pour éviter les patterns de friction détectés par `claude-doctor` sur ce projet : edit-thrashing, restart-cluster, repeated-instructions, negative-drift, error-loop, excessive-exploration.

### Editing discipline (anti edit-thrashing)

- Read the full file before editing. Plan all changes, then make ONE complete edit.
- If you've edited the same file 3+ times, STOP. Re-read the user's original requirements and re-plan from scratch.
- Prefer one large coherent edit over multiple small incremental ones.

### Stay aligned with the user (anti repeated-instructions, rapid-corrections)

- Re-read the user's last message before responding. Follow through on every instruction completely — don't partially address requests.
- Every few turns on a long task, re-read the original request to verify you haven't drifted from the goal.
- When the user corrects you: stop, re-read their message, quote back what they actually asked for, and confirm understanding before proceeding.

### Act, don't explore (anti excessive-exploration)

- Don't read more than 3-5 files before making a change. Get a basic understanding, make the change, then iterate.
- Prefer acting early and correcting via feedback over prolonged reading and planning.

### Break loops (anti error-loop, restart-cluster)

- After 2 consecutive tool failures or the same error twice, STOP. Change your approach entirely — don't retry the same strategy. Explain what failed and try something genuinely different.
- When truly stuck, summarize what you've tried and ask the user for guidance rather than retrying.

### Verify output (anti negative-drift)

- Before presenting your result, double-check it actually addresses what the user asked for.
- If the diff doesn't map cleanly to the user's request, don't ship it — re-plan.

## Claude Code Mechanics — Verified Citations

This appendix consolidates every Claude Code internal mechanism Distill depends on into a single table with `claude-code/<path>:<line>` citations. The upstream evolves; this table makes single-pass re-verification possible. If a citation no longer resolves, that is the first signal that upstream mechanics have shifted and Distill's assumptions need a recheck.

All citations resolve against the local copy of the Claude Code source at `/home/arthur/dev/claude-code/`. File paths are relative to that root. Line numbers are anchors into the function/constant of interest — if the exact line shifts a few positions upstream, grep by name (constant name, function name) to recover.

| # | Mechanism | Behavior | Citation(s) |
|---|-----------|----------|-------------|
| 1 | `_meta['anthropic/alwaysLoad']` semantics | MCP client reads the `_meta` flag into the internal `Tool.alwaysLoad`. Deferral logic bypasses ToolSearch when `alwaysLoad === true` (checked first, before any other rule). | `claude-code/services/mcp/client.ts:1785`; `claude-code/tools/ToolSearchTool/prompt.ts:60-65` (`isDeferredTool`) |
| 2 | `_meta['anthropic/searchHint']` — scoring only | Hint is a scoring signal inside ToolSearch (+4 match bonus) — it is **never rendered** in the deferred-tools list the model sees (`formatDeferredToolLine` returns `tool.name` only). The A/B for rendering hints (`exp_xenhnnmn0smrx4`, stopped 2025-03-21) showed no benefit. Combined with mechanism #1, this means `searchHint` is unreachable for tools that set `alwaysLoad: true`. | `claude-code/tools/ToolSearchTool/ToolSearchTool.ts:282-285`; `claude-code/tools/ToolSearchTool/prompt.ts:110-117` |
| 3 | `outputSchema` handling | Silently ignored during the `tools/list` → internal `Tool` mapping (mapper reads only `description`, `inputSchema`, `_meta.*`, `annotations.*`). Tools carrying `outputSchema` are neither dropped nor rejected — the field is simply not copied. | `claude-code/services/mcp/client.ts:1754-1813` |
| 4 | MCP tool-result persistence threshold | Token-based, not char-based. Hard ceiling `DEFAULT_MAX_MCP_OUTPUT_TOKENS = 25_000`. A heuristic gate (`contentSizeEstimate <= 25_000 * MCP_TOKEN_COUNT_THRESHOLD_FACTOR` where the factor is `0.5`, giving ≈ 12,500 tokens ≈ 50K chars via the `length/4` estimator) short-circuits before calling the real token-count API. The 50K-char `DEFAULT_MAX_RESULT_SIZE_CHARS` in `toolLimits.ts:13` is a **different** constant for **built-in** tools and does not gate MCP. | `claude-code/utils/mcpValidation.ts:14` (`MCP_TOKEN_COUNT_THRESHOLD_FACTOR`); `claude-code/utils/mcpValidation.ts:16` (`DEFAULT_MAX_MCP_OUTPUT_TOKENS`); `claude-code/utils/mcpValidation.ts:151-163` (`mcpContentNeedsTruncation`); `claude-code/services/mcp/client.ts:2720` (`processMCPResult`); `claude-code/constants/toolLimits.ts:13` (built-in only, for contrast) |
| 5 | Autocompact formula | `reserved = min(getMaxOutputTokensForModel(model), MAX_OUTPUT_TOKENS_FOR_SUMMARY)` where `MAX_OUTPUT_TOKENS_FOR_SUMMARY = 20_000`. Then `effectiveContextWindow = contextWindow - reserved`; trigger fires at `effectiveContextWindow - AUTOCOMPACT_BUFFER_TOKENS` where `AUTOCOMPACT_BUFFER_TOKENS = 13_000`. **Haiku with `max_output = 4_096` reserves only 4,096 — not 20,000 — so its trigger is ~16K tokens higher than a doc that hardcodes "20K reserved" would suggest.** | `claude-code/services/compact/autoCompact.ts:30` (`MAX_OUTPUT_TOKENS_FOR_SUMMARY`); `claude-code/services/compact/autoCompact.ts:33-48` (`getEffectiveContextWindowSize`); `claude-code/services/compact/autoCompact.ts:62` (`AUTOCOMPACT_BUFFER_TOKENS`); `claude-code/services/compact/autoCompact.ts:72-76` (`getAutoCompactThreshold`) |
| 6 | MCP → API block transport: `structuredContent` dropped | `transformResultContent` JSON-stringifies `structuredContent` and stashes it in `mcpMeta.structuredContent`. The `mcpMeta` field on the user message is explicitly documented as "MCP protocol metadata to pass through to SDK consumers (**never sent to model**)" — so `structuredContent` dies at the MCP boundary and never reaches the Anthropic API. Only the `content[].text` array (and image blocks) are transmitted. Practical consequence: populating `structuredContent` on tool responses pays serialization cost for a field the LLM will not see. | `claude-code/services/mcp/client.ts:2675-2684` (storage path); `claude-code/utils/messages.ts:482-486` (`mcpMeta` definition with the "never sent to model" docstring) |
| 7 | PreCompact hook dispatch | Only documented lever to influence autocompact behavior. `executePreCompactHooks` runs registered hook commands with a `PreCompactHookInput` on stdin. Hook stdout's `newCustomInstructions` is merged (via `mergeHookInstructions`) into the compact-summary prompt — so a hook is the single mechanism by which an external process can tell the summarizer LLM what to preserve or how to compact. | `claude-code/utils/hooks.ts:3961-4025` (`executePreCompactHooks`); `claude-code/services/compact/compact.ts:420-423` (`mergeHookInstructions`) |
| 8 | MCP prompts → slash commands | MCP `prompts/list` results are mapped to Claude Code commands with the naming convention `mcp__<normalized-server-name>__<prompt-name>`. Each registered prompt becomes a slash command in the session. Zero token cost when unused — prompts are lazy-loaded, not injected into the system prompt. | `claude-code/services/mcp/client.ts:2043-2060` (`prompts/list` → Command mapping) |
| 9 | Custom agent loading + gating | Agent markdown files loaded from `~/.claude/agents/*.md` (and project-local `.claude/agents/`) via `loadMarkdownFilesForSubdir('agents', cwd)`. Frontmatter fields relevant to Distill: `tools`, `disallowedTools`, `requiredMcpServers`. `hasRequiredMcpServers(agent, availableServers)` gates an agent out of the active list when any required MCP server pattern is not matched (case-insensitive substring). This means an agent declaring `requiredMcpServers: [distill-mcp]` silently disappears when Distill isn't connected — graceful degradation, no broken agent invocations. | `claude-code/tools/AgentTool/loadAgentsDir.ts:77` (Zod `disallowedTools`); `claude-code/tools/AgentTool/loadAgentsDir.ts:105-122` (`BaseAgentDefinition` shape with `requiredMcpServers`); `claude-code/tools/AgentTool/loadAgentsDir.ts:229-243` (`hasRequiredMcpServers` gating); `claude-code/tools/AgentTool/loadAgentsDir.ts:300-315` (markdown load path) |
| 10 | `annotations.readOnlyHint` → `isConcurrencySafe` | MCP client copies `tool.annotations.readOnlyHint` into the internal `Tool`'s `isConcurrencySafe()` (and `isReadOnly()`). Concurrency-safe tools can be dispatched in parallel with other read-only tools, reducing wall-clock latency on multi-tool turns. Default (when unset) is `false` — concurrency-safe behavior is strictly opt-in. | `claude-code/services/mcp/client.ts:1795-1800` |
| 11 | MCP skills — `loadedFrom === 'mcp'` (reference for EP-006 spike) | `SkillTool` filters `context.getAppState().mcp.commands` for `cmd.type === 'prompt' && cmd.loadedFrom === 'mcp'`. Skills surfaced via MCP become model-invokable through `SkillTool`. The server-side contract that produces `loadedFrom === 'mcp'` is not publicly documented and is the subject of the EP-006 spike. | `claude-code/tools/SkillTool/SkillTool.ts:82-93` |

**How to re-verify:** `rg 'DEFAULT_MAX_MCP_OUTPUT_TOKENS|AUTOCOMPACT_BUFFER_TOKENS|MAX_OUTPUT_TOKENS_FOR_SUMMARY|executePreCompactHooks|hasRequiredMcpServers|isConcurrencySafe|alwaysLoad' /home/arthur/dev/claude-code`. If a name disappears from the output, upstream has likely renamed or refactored it — re-trace the mechanism from scratch before trusting the corresponding Distill behavior.
