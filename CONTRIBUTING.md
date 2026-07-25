# Contributing to Distill

Distill is a native Rust context projection engine. Contributions should keep
projections bounded, recoverable, deterministic, and independent of host
protocols.

## Prerequisites

- Rust 1.97.1
- Bun 1.3+
- SQLite development libraries
- `cargo-llvm-cov` and `cargo-fuzz` for the complete native gate

## Setup

```bash
git clone https://github.com/arthjean/distill.git
cd distill
bun install
bun run build
```

The production crate is `native/distill-core`. The evaluation corpus lives in
`evaluation/corpus`, and release protocols and evidence live in
`evaluation/release`.

## Validation

Run the complete native gate for runtime changes:

```bash
bun run check:native
```

Run one focused test while iterating:

```bash
cargo test --manifest-path native/distill-core/Cargo.toml <test-name>
```

For root JavaScript manifests or evaluation tooling:

```bash
bun run knip
```

For distribution changes, build an unpublished package and verify its adjacent
checksum:

```bash
bun run package:native
```

Packaging writes ignored files under `dist/native`. It does not publish or
create a release.

## Pull requests

Create branches from `dev` and target every pull request to `dev`, never
`main`. Keep each change focused, add tests for changed behavior, update the
relevant contract documentation, and use Conventional Commits.

The main invariants are:

- commit source bytes before omission;
- keep host types and policy out of adapters;
- preserve configured-root, permission, integrity, resource, and zero-network
  guarantees;
- write machine protocol only to stdout and diagnostics to stderr;
- use executable plus argv without implicit shell parsing.

Historical PRDs, legacy evidence, qualification receipts, release notes, and
the release changelog are immutable outside their explicit workflows.

Publishing, version changes, release notes, tags, GitHub releases, and changes
to release automation require separate maintainer approval.

## License

Contributions are licensed under the MIT License.
