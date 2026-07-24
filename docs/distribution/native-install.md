# Distill native asset

This archive contains the `distill` local context projection binary for one
supported packaging platform. Packaging does not itself confer release
qualification; consult `docs/distribution/native-assets.json` in the source
repository for the current tree-bound status.

## Install

1. Verify the adjacent `.sha256` file after obtaining both files through the
   authenticated release channel.
2. Extract the archive.
3. Move `distill` to a directory on `PATH`, preserving executable mode.
4. Run `distill --help`.

The binary is non-interactive and performs no runtime network requests. Its
default local store is permission-restricted. Configure explicit roots for
`read`, `run`, and Claude MCP acquisition.

The Linux asset targets GNU libc and dynamically uses `libgcc_s` and the system
`libsqlite3.so.0`. The macOS asset uses the macOS system runtime and system
SQLite. These are the runtime libraries exercised by the supported packaging
hosts; the archives do not claim static or musl compatibility.

The checksum detects transfer corruption or substitution relative to the
checksum file. It does not authenticate the publisher by itself.

## Host setup

Codex automatic mode:

```bash
distill setup codex \
  --config /absolute/path/to/codex-config.json \
  --command /absolute/path/to/distill \
  --mode active
```

Claude Code explicit MCP mode:

```bash
distill setup claude \
  --config /absolute/path/to/claude-settings.json \
  --command /absolute/path/to/distill \
  --root workspace=/absolute/path/to/workspace
```

Both commands support `--dry-run` and `--restore`. Codex coverage is limited to
supported local-tool `PostToolUse` events. Claude native `Read` and `Bash` are
not intercepted; use `distill_read` and `distill_run`.

Packaging is limited to Linux x86_64 GNU and macOS arm64. The Linux asset still
requires a same-tree release qualification; the macOS asset is qualified for
the current native tree. No other platform is implied by the archive format.
