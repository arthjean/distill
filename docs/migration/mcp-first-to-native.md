# MCP-first to native migration

## Decision summary

The validated replacement is the native Rust `distill` binary. Distribution
uses Linux x86_64 and macOS arm64 executables embedded in the scoped
`@arthjean/distill` package. This new package is a distribution shell around
the native engine, not a restoration of the TypeScript MCP-first runtime.
US-020 removed that legacy package after the native qualification and migration
gates passed. The pre-deletion commit and recovery ref are recorded in
`legacy-deletion-plan.md`.

US-019 prepared the migration without deleting code or performing a release.
US-020 removed the legacy runtime and disabled its npm publication workflow.
Neither story published an asset, changed a version, created a release, or
created a tag.

The same-tree Linux and macOS qualification is `GO` in
`evaluation/release/evidence/native-distribution-v2.json` for candidate
`69bd3e5e712678a7c03f43b0f6e3e1dd4e2e65ba` and native tree
`77c75f741cd69a9b229dd73c1be435181db73cf1`. The historical v1 aggregate remains
`NO-GO`: it looked for native prevalidation in the automated receipt after that
field had been intentionally dropped during typed reserialization. V2 retains
every gate and reads the binding from the standalone suite receipt that owns it.

## Legacy tool mapping

| Legacy use case                                                                        | VNext disposition                      | Surface                | Migration                                                                                                                               |
| -------------------------------------------------------------------------------------- | -------------------------------------- | ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `auto_optimize` on a supported Codex local-tool result                                 | Replaced                               | Codex automatic        | Install `distill setup codex ... --mode active`; the `PostToolUse` adapter captures, persists, and projects the observation             |
| `auto_optimize` on manually supplied text                                              | Replaced                               | CLI                    | Pipe bytes to `distill project --budget N`; choose a versioned preservation profile when the default plain-text profile is insufficient |
| `auto_optimize` on Claude process output                                               | Replaced when Distill owns acquisition | Claude explicit        | Use `distill_run`; native Claude `Bash` is not intercepted                                                                              |
| `auto_optimize` on Claude file output                                                  | Replaced when Distill owns acquisition | Claude explicit        | Use `distill_read`; native Claude `Read` is not intercepted                                                                             |
| `auto_optimize` generative conversation or semantic summary                            | Removed                                | None                   | V1 is deterministic and extractive; no model-backed summarizer is retained                                                              |
| `smart_file_read` full-file bounded reading                                            | Replaced                               | CLI or Claude explicit | Use `distill read` or `distill_read` with an explicit root and budget                                                                   |
| `smart_file_read` exact whole-file retrieval                                           | Replaced                               | CLI                    | Use a sufficient budget or recover the committed artifact with `distill artifact get`                                                   |
| `smart_file_read` skeleton, symbol extraction, search, or seven-language AST structure | Deferred                               | Native agent tools     | Use the host's search/read tools or exact artifact recovery; AST-aware projection requires a later evidence-backed PRD                  |
| `code_execute` batching of native agent operations                                     | Removed                                | Host agent             | Use the host's native tools and orchestration                                                                                           |
| `code_execute` fixed local command execution                                           | Narrow replacement                     | CLI or Claude explicit | Use `distill run` or `distill_run` with a fixed executable and argv                                                                     |
| `code_execute` arbitrary TypeScript SDK or QuickJS sandbox                             | Removed                                | None                   | Programmable user projection is outside the bounded projection contract                                                                 |
| Process-scoped `restore_original` handle                                               | Replaced                               | Artifact CLI           | Use the durable artifact ID with `artifact get`, `artifact trace`, and `status` after restart                                           |
| Three always-loaded MCP tools                                                          | Removed                                | Host-specific adapters | Codex uses a hook; Claude exposes only `distill_read` and `distill_run`                                                                 |
| PreCompact compression markers                                                         | Removed                                | Receipts and artifacts | Projection accounting is explicit and independent of host compaction                                                                    |

No row is unresolved. Deferred AST behavior is an explicit non-goal, not a
silent compatibility promise.

## Resolved US-004 salvage matrix

| Legacy behavior                                      | US-004 class  | Resolution                                                       | Replacement evidence                                                                                                                                                  |
| ---------------------------------------------------- | ------------- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Deterministic repeatability of supported projections | `preserve`    | Preserved                                                        | `native/distill-core/src/projection.rs` test `spans_cover_source_once_and_output_is_deterministic` asserts 100 byte-identical outcomes; the 102-fixture corpus passes |
| Content-type routing and extractive reducers         | `re-evaluate` | Preserve the observable P0/P1 outcome, discard the legacy router | Linux gate: 101/101 P0 facts and 89/92 P1 facts preserved, no budget overrun; US-018 v5: 50/50 projected task successes                                               |
| AST-backed structural file projection                | `re-evaluate` | Deferred                                                         | US-004 measured 0% annotated P0/P1 recall for the structural output; v1 explicitly provides bounded byte projection instead                                           |
| Implicit token counter and fallback                  | `re-evaluate` | Replaced                                                         | `cl100k_base@js-tiktoken-1.0.15` is named in every token receipt; unknown profiles return `token_profile_unsupported` without fallback                                |
| Process-scoped origin store                          | `discard`     | Replaced by durable SQLite artifacts                             | Linux gate restores 102/102 artifacts after restart and 8/8 concurrent-writer artifacts with zero SHA-256 mismatch                                                    |
| Three always-loaded MCP tools                        | `discard`     | Removed                                                          | Codex hook and two explicit Claude acquisition tools pass Linux and macOS setup, repeat, restore, and uninstall checks                                                |
| QuickJS execution surface                            | `discard`     | Removed                                                          | The replacement exposes argv-only bounded execution and no programmable projection runtime                                                                            |
| Generative summarization                             | `discard`     | Removed                                                          | Central profiles are deterministic and extractive; paired projected tasks scored 50/50 without a model-backed reducer                                                 |
| Compression markers tied to host compaction          | `re-evaluate` | Removed                                                          | Versioned receipts account for visible and omitted spans; durable artifacts recover source independently of host compaction                                           |

The authoritative evidence is:

- `evaluation/legacy/baseline.md`;
- `evaluation/release/evidence/automated-linux-x86_64-v2.json`;
- `evaluation/release/evidence/suite-linux-x86_64-v2.json`;
- `evaluation/release/evidence/macos-arm64-distribution-v1.json`;
- `evaluation/release/evidence/native-distribution-v2.json`;
- `evaluation/release/evidence/us018-qualification-v5.json`.

The sole US-004 `preserve` row has a passing replacement test and does not block
migration.

## Automatic and explicit modes

### Codex

`active` mode automatically projects supported local-tool `PostToolUse` events.
`observe` records bounded diagnostics without replacing visible output. `off`
leaves events unchanged.

Coverage is defined by
`docs/integrations/codex-hook-conformance-v2.json`. Hosted tools and specialized
paths that emit no supported event are blind spots. Distill cannot emit a
warning for an event it never receives.

### Claude Code

Claude projection is explicit. The MCP adapter owns file or process acquisition
through `distill_read` and `distill_run`. Native `Read` and `Bash` bypass
Distill. There is no universal Claude interception claim.

## Distribution preparation

US-019 initially selected two direct native assets:

| Target           | Archive                       | Qualification                         |
| ---------------- | ----------------------------- | ------------------------------------- |
| Linux x86_64 GNU | `distill-linux-x86_64.tar.gz` | Same-tree native distribution v2 `GO` |
| macOS arm64      | `distill-macos-arm64.tar.gz`  | Same-tree native distribution v2 `GO` |

`./scripts/package-native.sh` builds with the locked Rust dependency graph,
packages the binary, license, and install guide, and writes an adjacent SHA-256
file. It rejects every unsupported packaging host. Generated archives stay
under ignored `dist/native/`. Packaging does not promote a target's
qualification status.

The packaged executables use the system runtime and SQLite libraries on their
host. Linux targets GNU libc with `libgcc_s` and `libsqlite3.so.0`; macOS uses
the macOS system runtime and SQLite. Static and musl distribution remain
unsupported.

The machine-readable contract is
`docs/distribution/native-assets.json`. No npm launcher, registry publish,
release tag, version bump, changelog entry, or workflow change was part of
US-019. The later native npm requirement changed the distribution strategy
without changing those historical facts.

The v2 aggregate reuses the immutable Linux and macOS receipts and binds both
platforms to native tree `77c75f741cd69a9b229dd73c1be435181db73cf1`.
The remote attestation
`refs/distill/qualifications/native-distribution-v2-20260725` points to the
qualified candidate. Qualification does not publish either asset.

The npm launcher decision was reopened for the explicit native npm installation
requirement. Version 0.1.0 embeds both qualified executables in one atomic
package, performs no `postinstall` download, and uses a POSIX launcher so Node
is not part of the runtime process boundary. npm rejects other operating
systems. The launcher selects Linux x86_64 only with GNU libc, selects macOS
arm64, and rejects unsupported architectures and Linux runtimes before invoking
a binary.

## Unsupported platform matrix

| Platform or host surface                  | Status      | Reason and user-visible behavior                                     |
| ----------------------------------------- | ----------- | -------------------------------------------------------------------- |
| Linux x86_64 GNU                          | Supported   | Same-tree native asset qualified                                     |
| macOS arm64                               | Supported   | Qualified native asset                                               |
| Windows x86_64 and arm64                  | Unsupported | No build, permission, setup, recovery, or release execution evidence |
| macOS x86_64                              | Unsupported | Outside v1 gate and not executed                                     |
| Linux arm64                               | Unsupported | Outside v1 gate and not executed                                     |
| Linux x86_64 musl                         | Unsupported | GNU target only; no musl or static-link evidence                     |
| Other Unix targets                        | Unsupported | No release asset or qualification                                    |
| Codex supported local `PostToolUse`       | Supported   | Automatic hook mode                                                  |
| Codex hosted tool without supported event | Unsupported | No interception and no possible Distill diagnostic                   |
| Claude `distill_read` and `distill_run`   | Supported   | Explicit MCP acquisition                                             |
| Claude native `Read` and `Bash`           | Unsupported | Tool output bypasses Distill                                         |
| Cursor, Windsurf, Continue                | Unsupported | Legacy setup is not carried forward                                  |
| Generic MCP client                        | Unqualified | Protocol compatibility is not a v1 support claim                     |
| Interactive shell, PTY, remote process    | Unsupported | `run` is bounded local executable plus argv only                     |

## Deletion gate record

Before deleting the first file, US-020 verified:

1. US-017 and US-018 remain `GO`.
2. Native distribution v2 remains `GO`, and the real HEAD/worktree native tree
   still equals its recorded native tree.
3. The exact deletion inventory in `legacy-deletion-plan.md` still matches the
   repository.
4. No production import outside `packages/mcp-server` resolves through the
   legacy package.
5. Corpus, legacy baseline, release evidence, and historical PRDs are retained.
6. CI and release workflow edits have separate human authorization.
7. Publishing and version changes remain separately authorized release actions.

All preconditions passed. The deletion remained one controlled slice with no
partial compatibility layer or opportunistic cleanup.
