<p align="center">
  <img src="assets/distill-logo.svg" alt="Distill" width="130" />
</p>

<h1 align="center">Distill</h1>

<p align="center">
  <b>Bounded, recoverable context projection for coding agents.</b>
</p>

Distill is a local native engine that captures tool observations, commits the
raw bytes, and returns a deterministic model-visible projection within an
explicit budget. Every reduced result carries a durable artifact reference and
a receipt describing what was retained or omitted.

The native engine is qualified on Linux x86_64 and macOS arm64 on the same
native tree. Nothing has been published. The retired TypeScript MCP-first
implementation remains recoverable through the recorded pre-US-020 ref, while
its historical evidence stays in the repository.

## Product contract

```mermaid
flowchart LR
    observation["Observation"] --> commit["Commit raw bytes"]
    commit --> project["Project to budget"]
    project --> result["Visible output<br/>Artifact reference<br/>Receipt"]
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

Requirements: Rust 1.97.1 and the platform SQLite development libraries.

```bash
cargo build --locked --release --manifest-path native/distill-core/Cargo.toml
./native/distill-core/target/release/distill --help
```

Evaluation and qualification tooling additionally requires Bun 1.3+. It does
not require a package installation.

Create an unpublished archive for the current supported packaging host:

```bash
./scripts/package-native.sh
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
| Linux x86_64 GNU                                  | Qualified          | Native archive                                                 |
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

The native distribution v2 aggregate binds the Linux and macOS receipts to
native tree `77c75f741cd69a9b229dd73c1be435181db73cf1`. The Linux release gate
covers 102 corpus fixtures, recovery, concurrency, latency, memory, fuzzing,
and zero-network behavior. The US-018 v5 paired gate scored 50/50 for raw and
50/50 for projected conditions with a 0 point delta. No qualification action
published an asset, tag, or version.

## Development

Native verification:

```bash
./scripts/check-native.sh
bun evaluation/corpus/check.mjs --verify
```

Build an unpublished native archive and verify its adjacent checksum:

```bash
./scripts/package-native.sh
```

The approved deletion and rollback record is
[`docs/migration/legacy-deletion-plan.md`](docs/migration/legacy-deletion-plan.md).

## License

[MIT](LICENSE)
