<p align="center">
  <img src="assets/distill-logo.png" alt="Distill" width="130" />
</p>

<h1 align="center">Distill</h1>

<p align="center">
  <b>Bounded, recoverable context projection for coding agents.</b>
</p>

Distill is a local native engine that captures tool observations, commits the
raw bytes, and returns a deterministic model-visible projection within an
explicit budget. Every reduced result carries a durable artifact reference and
a receipt describing what was retained or omitted.

The vNext release candidate is qualified on macOS arm64. Linux x86_64 packaging
is prepared, but the committed Linux `GO` predates the current native tree, so a
same-tree Linux qualification remains a release prerequisite. Nothing has been
published. The existing `distill-mcp` npm package remains the frozen legacy
product until its separately approved retirement.

## Product contract

```text
observation
    |
    v
commit raw bytes -> project to budget -> visible output + artifact ref + receipt
```

The engine:

- persists source bytes before returning an omitting projection;
- restores unexpired artifacts after process restart and verifies SHA-256;
- applies versioned byte or token budgets to the complete visible envelope;
- emits versioned JSON for machine consumers and diagnostics only on stderr;
- performs no runtime network requests;
- treats captured output as untrusted data, never as commands or configuration.

The central Rust module has no Codex, Claude, MCP, or JSON-RPC types. Host
adapters translate protocols and acquisition only.

## Build from source

Requirements: Rust 1.97.1 and Bun 1.3+.

```bash
bun install
bun run build:native
./native/distill-core/target/release/distill --help
```

Create an unpublished archive for the current supported packaging host:

```bash
bun run package:native
```

This produces `dist/native/distill-<platform>.tar.gz` and its SHA-256 file. It
does not publish, tag, change a version, or qualify the resulting source tree.

The Linux archive targets GNU libc and uses the host's `libgcc_s` and
`libsqlite3.so.0`. The macOS archive uses the macOS system runtime and SQLite.
No static or musl compatibility is claimed.

## CLI

Project stdin into a bounded result:

```bash
printf 'large tool output' |
  distill project --budget 2048 --unit bytes --json
```

Read a file through an allowlisted root:

```bash
distill \
  --root workspace=/absolute/path/to/project \
  read --root-id workspace --path src/main.rs --budget 4096 --json
```

Run a non-interactive process without a shell:

```bash
distill \
  --root workspace=/absolute/path/to/project \
  run --cwd-root workspace --cwd . --budget 4096 -- cargo test
```

Recover and inspect artifacts:

```bash
distill artifact get <artifact-id> --json
distill artifact trace <artifact-id> --json
distill status --json
distill gc --json
```

All commands are non-interactive. `run` accepts an executable and argv only: no
shell interpolation, PTY, remote execution, or general process supervision.

## Codex: automatic local-tool projection

Install the versioned `PostToolUse` hook:

```bash
distill setup codex \
  --config /absolute/path/to/codex-config.json \
  --command /absolute/path/to/distill \
  --mode active
```

Modes are `off`, `observe`, and `active`. Setup requires an explicit config
path, supports `--dry-run`, preserves the first-install bytes, is idempotent,
and supports `--restore`.

Automatic projection covers only supported local-tool events that Codex emits
through `PostToolUse`. Hosted tools and specialized paths that emit no supported
event are invisible to Distill, so Distill cannot diagnose them.

## Claude Code: explicit projected acquisition

Install the stdio MCP adapter:

```bash
distill setup claude \
  --config /absolute/path/to/claude-settings.json \
  --command /absolute/path/to/distill \
  --root workspace=/absolute/path/to/project
```

Claude receives two explicit tools:

- `distill_read`: bounded file acquisition under a configured root;
- `distill_run`: bounded argv-only process acquisition.

Distill does not intercept Claude's native `Read` or `Bash`. Selecting those
tools bypasses projection. MCP stdout contains JSON-RPC only.

## Supported and unsupported surfaces

| Surface                                           | V1 status          | Contract                                                       |
| ------------------------------------------------- | ------------------ | -------------------------------------------------------------- |
| Linux x86_64 GNU                                  | Packaging prepared | Same-tree release qualification pending                        |
| macOS arm64                                       | Qualified          | Native archive                                                 |
| Codex supported local `PostToolUse` events        | Qualified          | Automatic off, observe, or active mode                         |
| Claude `distill_read` and `distill_run`           | Qualified          | Explicit MCP acquisition                                       |
| Codex hosted tools without a supported hook event | Unsupported        | No interception and no Distill diagnostic                      |
| Claude native `Read` and `Bash`                   | Unsupported        | Bypass Distill                                                 |
| Windows, macOS x86_64, Linux arm64, Linux musl    | Unsupported        | No release claim                                               |
| Cursor, Windsurf, Continue, generic MCP clients   | Unqualified        | No v1 setup or compatibility claim                             |
| AST-aware code extraction                         | Deferred           | Use exact or bounded file projection                           |
| Programmable `code_execute` sandbox               | Removed from vNext | Use native agent tools or `distill run` for a fixed executable |

The complete host and platform matrix is in
[`docs/migration/mcp-first-to-native.md`](docs/migration/mcp-first-to-native.md).

## Architecture and evidence

- [Native architecture](docs/architecture/native-context-projection-engine.md)
- [Host integration contract](docs/integrations/native-surfaces.md)
- [Threat model](docs/security/context-projection-threat-model.md)
- [Migration from the MCP-first product](docs/migration/mcp-first-to-native.md)
- [Native distribution manifest](docs/distribution/native-assets.json)
- [Release qualification](evaluation/release/README.md)

The earlier Linux release gate covers 102 corpus fixtures, recovery,
concurrency, latency, memory, fuzzing, and zero-network behavior, but its native
tree is recorded separately from the current tree. The US-018 v5 paired gate
scored 50/50 for raw and 50/50 for projected conditions with a 0 point delta.
The matching current-tree macOS arm64 candidate passed the contract, corpus,
setup, restore, and uninstall suite.

## Development

Native verification:

```bash
./scripts/check-native.sh
bun run check:migration
```

The repository still contains the frozen TypeScript MCP package as migration
evidence. Its validation remains:

```bash
bun run check-types
bun run lint
cd packages/mcp-server && bun run test
```

No TypeScript legacy deletion begins without explicit maintainer approval after
the migration plan is reviewed.

## License

[MIT](LICENSE)
