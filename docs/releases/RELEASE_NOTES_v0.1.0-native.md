# Distill native v0.1.0

Distill v0.1.0 starts the version line for the replacement native engine and
the public `@arthjean/distill` package. It does not restore or continue the
retired TypeScript `distill-mcp` runtime.

## Installation

```bash
npm install --global @arthjean/distill
distill --help
```

The package embeds both supported executables. Installation performs no
download or compilation.

## Supported platforms

- Linux x86_64 with GNU libc, `libgcc_s`, and system `libsqlite3.so.0`
- macOS arm64 with the macOS system runtime and system SQLite

Every other OS and architecture pair fails explicitly in the package launcher.

## Distribution contract

Both executables must come from the same clean source tree and pass the
versioned native qualification before publication. The package assembler
verifies adjacent archive checksums, platform executable identity, qualification
receipt bindings, and embedded binary SHA-256 values before creating the npm
tarball.

The executable performs no runtime network requests. Source capture, artifact
persistence and verification, projection, receipts, setup, restore, trace, and
garbage collection remain inside the native engine contract.
