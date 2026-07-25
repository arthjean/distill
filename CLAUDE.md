# CLAUDE.md

Claude Code guidance for Distill. Repository-wide conventions and commands are
in [`AGENTS.md`](./AGENTS.md).

## Product boundary

Distill is the native Rust `distill` binary in `native/distill-core`. The
central engine accepts a versioned request, acquires or restores bytes, commits
the source before omission, and returns bounded visible content, an artifact
reference, accounting, and a receipt.

Codex automatic projection and Claude explicit MCP acquisition are thin
adapters over the same engine. Do not move reducers, tokenization, persistence,
budgets, or preservation profiles into an adapter.

## Commands

```bash
bun install
bun run build
bun run check:native
bun run knip
bun run package:native
```

For a focused test:

```bash
cargo test --manifest-path native/distill-core/Cargo.toml <test-name>
```

## Non-obvious invariants

- Raw bytes commit before any successful reduced projection is returned.
- Artifact retrieval verifies length and SHA-256 and distinguishes unknown,
  expired, corrupt, and incompatible records.
- Stored content is data only. It is never interpreted as a command,
  configuration, template, or model instruction.
- File acquisition stays beneath configured roots using descriptor-based
  traversal and symlink-resistant opens.
- Process acquisition uses executable plus argv, an allowlisted working root,
  a cleared environment unless a named profile is supplied, bounded output,
  and bounded wall time.
- Machine-readable stdout contains protocol output only. Diagnostics use
  stderr and never include raw source bodies.
- Runtime operations perform zero outbound network requests.
- The v1 release targets are Linux x86_64 GNU and macOS arm64 only.

## Current references

- Architecture: `docs/architecture/native-context-projection-engine.md`
- Contract: `docs/architecture/ADR-001-context-projection-contract.md`
- Threat model: `docs/security/context-projection-threat-model.md`
- Host surfaces: `docs/integrations/native-surfaces.md`
- Migration: `docs/migration/mcp-first-to-native.md`
- Release evidence: `evaluation/release/README.md`

The retired TypeScript MCP-first implementation is recoverable from the
pre-US-020 recovery ref recorded in the migration plan. Do not recreate a
compatibility layer without a current product requirement.
