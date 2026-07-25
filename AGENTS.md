# AGENTS.md

Guidance for coding agents working in this repository.

## Project overview

Distill is a local native context projection engine. It captures tool
observations, commits source bytes before omission, and returns a deterministic
bounded projection with a durable artifact reference and receipt.

The product is the Rust `distill` binary in `native/distill-core`. Codex uses a
`PostToolUse` adapter for supported local tools. Claude Code uses the explicit
`distill_read` and `distill_run` MCP tools. Host adapters translate protocols
and acquisition only. Projection, tokenization, persistence, and preservation
policy remain in the central module.

## Repository layout

- `native/distill-core/`: production Rust crate, CLI, adapters, and tests.
- `evaluation/corpus/`: annotated projection corpus and language-independent
  oracle.
- `evaluation/release/`: qualification protocols, immutable evidence, and
  release-gate tooling.
- `docs/architecture/`, `docs/security/`, `docs/integrations/`: current product
  contracts.
- `docs/migration/`: legacy disposition and rollback record.

## Toolchains and commands

Rust `1.97.1` is pinned in `native/distill-core/rust-toolchain.toml`. Native
coverage and fuzz builds use `nightly-2026-07-19`. Bun is used only for root
orchestration and JavaScript tooling.

```bash
bun install
bun run build
bun run check:native
bun run knip
bun run package:native
```

`bun run check:native` runs Rust formatting, Clippy with warnings denied,
coverage-backed tests, branch and line floors, the corpus, and a fuzz-harness
build. Use `cargo test --manifest-path native/distill-core/Cargo.toml <name>`
for one focused test.

`bun run package:native` creates an unpublished archive and checksum under
ignored `dist/native/`. It does not publish, tag, release, or change a version.

## Code conventions

- Preserve the one-operation engine seam and keep host protocol types out of
  the central module.
- Use typed `Result` failures. Do not use production `unwrap()` or `expect()`
  unless a documented invariant makes failure unreachable.
- Keep machine-readable stdout protocol-only. Diagnostics go to stderr and
  must not contain raw source bodies or configured secrets.
- Process execution accepts an executable and argv. Never introduce implicit
  shell parsing, a PTY, remote execution, or stored-content execution.
- File acquisition must remain descriptor-based beneath configured roots and
  resist traversal, symlink replacement, and embedded NUL.
- Source bytes must commit atomically before any omitting projection or valid
  artifact reference is returned.
- Runtime capture, projection, restore, trace, and garbage collection make zero
  outbound network requests.
- Local defaults remain directory mode `0700`, data mode `0600`, seven-day
  retention, 512 MiB store cap, 10 MiB observations, and eight writers unless
  an approved contract change says otherwise.
- Use Bun, never npm, pnpm, yarn, or npx, for JavaScript package operations.
- Commits use Conventional Commits.

## Historical integrity

Do not rewrite historical evidence:

- `tasks/prd-distill-*.md`
- `evaluation/legacy/**`
- qualification evidence and protocols under `evaluation/release/**`
- `docs/releases/**`
- `CHANGELOG.md` until a separately authorized release

Migration documents may describe removed legacy names as historical facts.
They are not active runtime dependencies.

## Delivery

- Pull requests target `dev`, never `main`.
- Before delivery, inspect the complete diff and run the smallest complete gate
  bundle for the changed surface.
- Native runtime changes require `bun run check:native`.
- Manifest or evaluation JavaScript changes require `bun run knip`.
- Packaging changes require `bun run package:native` plus checksum validation.
- Workflow changes require explicit maintainer approval and a security review.
- Publishing, version bumps, changelog release entries, releases, and tags
  require separate explicit authorization.

## Critical safety rules

Never weaken storage permissions, artifact integrity, configured-root checks,
resource limits, output budgets, or zero-network behavior to make a test pass.
Never edit app-managed state, caches, sessions, `.codex/**`, or `.env*`. Never
commit secrets.
