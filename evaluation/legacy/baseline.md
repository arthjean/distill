# Legacy Distill baseline

- Evidence schema: `distill.legacy-baseline/v1`
- Generated: `2026-07-24T10:25:17.316Z`
- Git baseline: `9afe1d5594469c5937fa21e38f8adb6b547e8af6`
- Corpus: 102 fixtures, manifest SHA-256 `3e16733392422d43ea3697fbf54b0ddddb9424dcf46f4e3d2ca1f61dcbfb4f40`

This report is generated from `evidence.json`. Production and historical PRD files were measured but not edited. LOC is context only and is not a success criterion.

## Reference machine and protocol

| Field | Value |
|---|---|
| Platform | linux 7.1.4-204.fc44.x86_64 (x64) |
| CPU | 16 x AMD Ryzen 7 7800X3D 8-Core Processor |
| Memory | 32746606592 bytes |
| Bun | 1.3.14 |
| Node compatibility | v24.3.0 |
| Timing | 2 warm-ups, then 10 measured runs per timed case |
| Clock | performance.now, milliseconds |
| Peak RSS | process.memoryUsage().rss sampled after each measured run |
| Token profile | cl100k_base via js-tiktoken 1.0.15 gpt-4 mapping |

## Legacy path summary

| Path | Coverage | P0 recall | P1 recall | Median visible-token reduction | Determinism | Recovery or failure mode |
|---|---:|---:|---:|---:|---:|---|
| `auto_optimize` | 92/102 measured | 82.4% | 70.7% | 89.7% | 92/92 | {"requires_utf8_string":8} |
| `smart_file_read` | 16/16 file fixtures | 0.0% | 0.0% | 44.9% | 16/16 | No durable source artifact |
| Process-scoped recovery | 1 behavior probe | n/a | n/a | n/a | content-derived handle | same process: true; fresh process: false |
| Codex interception | unsupported | n/a | n/a | n/a | n/a | no_supported_codex_result_interception_path |

`auto_optimize` receives process observations as flattened text. It does not preserve stdout/stderr event order, exit code, signal, timeout, working directory, or truncation state. Malformed-byte fixtures are unsupported because the legacy interface requires a JavaScript string.

## Timed cases

| Case | Input bytes | Median ms | P95 ms | Peak RSS bytes | Valid |
|---|---:|---:|---:|---:|---|
| `auto:build-output-01` | 1737 | 0.634 | 0.661 | 383373312 | true |
| `auto:diff-01` | 2561 | 0.631 | 0.681 | 384159744 | true |
| `auto:unicode-01` | 3823 | 5.317 | 5.658 | 388091904 | true |
| `auto:boundary-one-mib` | 1048576 | invalid | invalid | 278429696 | false |
| `smart:source-code-01` | 2185 | 1.244 | 3.972 | 275701760 | true |
| `smart:json-01` | 5812 | 0.933 | 1.272 | 276881408 | true |
| `recovery:process-scoped-put-get` | 1737 | 0.002 | 0.005 | 377339904 | true |

Every valid row retains its two warm-up output digests, ten raw durations, ten output digests, ten RSS samples, and per-run errors in `evidence.json`. Invalid rows are not averaged.

## Recovery behavior

The optional origin store recovered SHA-256-identical bytes inside one process: true. A fresh process reported `missing`, so restart recovery is false. The measured LRU entry cap was 64; the measured byte cap was 33554432 bytes. Its failure mode is `origin_missing_after_process_restart_or_lru_eviction`.

## LOC context

Method: Physical TypeScript lines under packages/mcp-server/src; *.test.ts and type-tests.ts are tests.

| Class | Files | Physical lines |
|---|---:|---:|
| Production TypeScript | 128 | 34075 |
| Test TypeScript | 59 | 16855 |

These counts describe migration size only. They are not a projection-quality target.

## Salvage matrix

| Legacy behavior | Classification | Evidence |
|---|---|---|
| Deterministic repeatability of supported projections | `preserve` | 92/92 auto_optimize and 16/16 smart_file_read cases produced identical consecutive outputs |
| Content-type routing and extractive reducers | `re-evaluate` | Corpus P0 recall 82.4% and P1 recall 70.7% do not satisfy the replacement fidelity gate |
| AST-backed structural file projection | `re-evaluate` | Corpus P0 recall 0.0% and P1 recall 0.0%; structure can omit body facts |
| Current implicit token counter and fallback | `re-evaluate` | The legacy path maps gpt-4 to cl100k_base but does not expose a contract-bound tokenizer version or fallback status in every result |
| Process-scoped origin store | `discard` | Same-process recovery true; fresh-process recovery false |
| Three always-loaded MCP tools | `discard` | The host owns context aggregation and the new contract is one engine operation, not a fixed tool-count invariant |
| QuickJS execution surface | `discard` | Execution is outside bounded context projection and adds an unrelated security boundary |
| Generative summarization | `discard` | The frozen contract requires deterministic extractive projection and versioned fact preservation |
| Compression markers tied to host compaction | `re-evaluate` | Markers are adapter-specific envelope bytes and must be justified against explicit total-visible budget accounting |

## Invalid measurements

3 invalid measurement(s) remain in raw evidence and are excluded from aggregates: `corpus:auto:boundary-one-mib`, `corpus:auto:boundary-ten-mib`, `timing:auto:boundary-one-mib`.

