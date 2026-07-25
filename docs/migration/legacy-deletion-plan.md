# Reversible legacy deletion plan

## Scope and authority

This is the US-019 plan for a future US-020. It performs no deletion.

Linux x86_64 and macOS arm64 distribution are same-tree qualified by
`evaluation/release/evidence/native-distribution-v2.json`. This plan records
that gate but performs no publication or deletion.

The frozen inventory is anchored at commit
`704d50d6c1beb82abe442458a5e90eeac0611287`, immediately after US-018 v5
evidence was committed. The `packages/mcp-server` tree is
`2404877efa35c5917883389eb75a62af5c3ed571`.

The authoritative file-level inventory is
[`legacy-deletion-files.txt`](legacy-deletion-files.txt):

- 208 tracked files;
- sorted path-list SHA-256
  `10fb9d098c96c656dedce9750c84083677837ac4ff8f49bdccb89c39f05cc288`;
- each row is one path and one future `delete` action;
- any inventory drift blocks deletion until this plan is reviewed again.

## Required authorization

Before US-020 changes the first file, record:

1. explicit maintainer approval for legacy deletion after US-017 and US-018
   remain `GO`;
2. a native distribution v2 `GO` whose recorded native tree exactly matches the
   pre-deletion candidate;
3. separate approval for removing the pinned `web-tree-sitter@0.22.6` and
   `@sebastianwessel/quickjs@3.0.0` dependency entries;
4. separate approval for any `.github/workflows/**` edit;
5. a clean pre-deletion commit to use as the rollback source.

Publishing, version bumps, changelog release entries, and workflow edits are not
implied by deletion approval.

## US-020 execution record

Arthur explicitly authorized the legacy deletion, removal of the pinned
dependencies, and required workflow edits on 2026-07-25 after the native
distribution v2 aggregate remained `GO`.

The clean pre-deletion commit is
`19f079fce7248a7d2c33891ebf7a027c1decbd72`. The durable remote recovery ref is
`refs/heads/recovery/pre-us020-20260725` at that exact commit.

The final pre-deletion `bun run check:migration` completed with `VALID` before
the first deletion. It confirmed:

- 208 tracked legacy files;
- sorted path-list SHA-256
  `10fb9d098c96c656dedce9750c84083677837ac4ff8f49bdccb89c39f05cc288`;
- native distribution v2 `GO`;
- qualified Linux x86_64 and macOS arm64 evidence;
- native tree `77c75f741cd69a9b229dd73c1be435181db73cf1`.

## Obsolete tracked package

Every path in `legacy-deletion-files.txt` is obsolete at runtime after the
native product replaces the MCP-first package.

| Group                      | File-level disposition                                                                               | Replacement                                                           |
| -------------------------- | ---------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Package entry and metadata | Delete `package.json`, `server.json`, `bin/cli.js`, TypeScript and ESLint configs                    | Native `distill` binary and direct asset manifest                     |
| MCP server and registry    | Delete `src/server.ts`, `src/index.ts`, `src/tools/registry.ts`, prompts, constants, and their tests | Codex hook plus Claude `distill_read` and `distill_run`               |
| `auto_optimize`            | Delete tool, strategies, schemas, compressors, summarizers, parsers, fixtures, and tests             | Central deterministic projection profiles                             |
| `smart_file_read`          | Delete tool, AST implementations, caches, language support, fixtures, snapshots, and tests           | Bounded native `read`, exact artifact recovery, AST behavior deferred |
| `code_execute`             | Delete tool, QuickJS runtime, SDK, security bridge, analyzer, pipeline, and tests                    | Host-native tools or bounded argv-only `distill run`                  |
| Process origin recovery    | Delete `src/retrieve/**` and tests                                                                   | Durable SQLite artifact store                                         |
| PreCompact and setup hooks | Delete package scripts, agent asset, CLI setup/doctor/analyze/precompact modules and tests           | Native Codex and Claude setup, receipts, and artifacts                |
| Package documentation      | Delete package README and SDK reference                                                              | Root product README, native architecture, and migration docs          |
| GitHub Action wrapper      | Delete `packages/mcp-server/action/action.yml`                                                       | No v1 GitHub Action surface                                           |

The inventory includes every co-located test. No test is moved merely to
preserve an implementation detail. Observable behaviors retained by the
salvage matrix are already represented by native tests and corpus fixtures.

## Dependency disposition

Deleting `packages/mcp-server/package.json` removes these runtime dependency
edges:

`@clack/prompts`, `@jitl/quickjs-ng-wasmfile-release-sync`,
`@modelcontextprotocol/sdk`, `@sebastianwessel/quickjs`, `js-tiktoken`,
`neverthrow`, `safe-regex2`, `tree-sitter-wasms`, `typescript`,
`web-tree-sitter`, and `zod`.

It also removes package-local development edges:

`@repo/eslint-config`, `@repo/typescript-config`, `@types/node`,
`@vitest/coverage-v8`, `eslint`, and `vitest`.

US-020 must then make these exact root changes:

| File                      | Planned edit                                                                                                                                                                                                                                                                                                          | Rollback                                               |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------ |
| `package.json`            | After the final pre-deletion check, remove `dev:mcp`, `dev`, `lint`, `check-types`, and `check:migration`; repoint `build` to `build:native`; retain `format`, `knip`, `build:native`, and `package:native`; add `check:native`; remove root dev dependencies `distill-mcp`, `expect-type`, `turbo`, and `typescript` | Restore file from pre-deletion commit                  |
| `bun.lock`                | Regenerate with Bun after package deletion; verify no `distill-mcp` workspace or package-only dependency remains                                                                                                                                                                                                      | Restore lockfile, then `bun install --frozen-lockfile` |
| `knip.jsonc`              | Remove the `packages/mcp-server` workspace and its ignores; remove root ignores for `distill-mcp` and `expect-type`; remove `evaluation/legacy/run.mjs` as an entry and explicitly ignore that archival, intentionally non-executable recipe so the root `evaluation/**/*.mjs` project glob cannot parse it           | Restore file                                           |
| `turbo.json`              | Delete after root Turbo scripts and the `turbo` dev dependency are removed                                                                                                                                                                                                                                            | Restore file                                           |
| `smithery.yaml`           | Delete legacy npm stdio registration                                                                                                                                                                                                                                                                                  | Restore file                                           |
| `scripts/check-us019.mjs` | Run once as the final pre-deletion invariant, record its `VALID` result, then delete it because its contract intentionally requires the frozen legacy tree to exist                                                                                                                                                   | Restore file                                           |

No dependency version is upgraded or substituted during deletion.

## Generated outputs

After tracked removal, and only inside US-020, remove these reproducible
untracked outputs if present:

| Path                                        | Reason                     | Recovery                                                    |
| ------------------------------------------- | -------------------------- | ----------------------------------------------------------- |
| `packages/mcp-server/dist/`                 | TypeScript build output    | Restore by checking out legacy source and running its build |
| `packages/mcp-server/coverage/`             | Vitest coverage output     | Restore by rerunning legacy coverage                        |
| `packages/mcp-server/node_modules/`         | Workspace dependency links | Restore with Bun from the pre-deletion lockfile             |
| `.turbo/` entries for `packages/mcp-server` | Task cache                 | Recomputed automatically                                    |

The future deletion command must resolve each path explicitly. It must not use a
recursive target derived from an unset variable, the workspace root, `$HOME`,
or `~`.

## Documentation and harness disposition

| File                                                | Planned decision                                                                                                                               |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `README.md`                                         | Retain the US-019 native product README                                                                                                        |
| `CONTRIBUTING.md`                                   | Replace legacy development and test commands with native commands                                                                              |
| `AGENTS.md`                                         | Replace MCP-first repository instructions with native architecture and safety rules                                                            |
| `CLAUDE.md`                                         | Replace live MCP-first guidance; retain any historical internals only in an explicitly historical document                                     |
| `CHANGELOG.md`                                      | Retain byte-identical until a separately authorized release                                                                                    |
| `docs/releases/*.md`                                | Retain as historical release evidence                                                                                                          |
| `docs/migration/**`                                 | Retain as the migration decision and rollback record                                                                                           |
| `evaluation/legacy/baseline.md` and `evidence.json` | Retain byte-identical as US-004 evidence                                                                                                       |
| `evaluation/legacy/run.mjs`                         | Retain byte-identical as an archival measurement recipe; after source deletion it is intentionally non-executable and is not a release fixture |
| `evaluation/corpus/**`                              | Retain as the native release corpus                                                                                                            |
| `evaluation/release/**`                             | Retain all v1 through v5 evidence and qualification protocols                                                                                  |
| `tasks/prd-distill-*.md`                            | Retain byte-identical                                                                                                                          |
| `packages/eslint-config/**`                         | Retain; cleanup is unrelated to legacy runtime deletion                                                                                        |
| `packages/typescript-config/**`                     | Retain; cleanup is unrelated to legacy runtime deletion                                                                                        |

## Workflow decisions requiring separate approval

No workflow is modified by US-019.

| Workflow                                           | Required future decision                                                                                                               |
| -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `.github/workflows/build.yml`                      | Replace the MCP coverage and shellcheck jobs with native checks, and remove legacy-only Bun jobs only after explicit workflow approval |
| `.github/workflows/release.yml`                    | Disable or replace npm publishing before its package path disappears; publishing a native release remains separately approved          |
| `.github/workflows/native-macos-qualification.yml` | Retain unchanged unless a later release design needs asset packaging                                                                   |

US-020 must stop before deletion if workflow authorization is absent. Leaving a
required workflow pointing at a deleted path is not an acceptable intermediate
state.

## Ordered execution

1. Verify the inventory count, path-list hash, historical PRD hashes, clean
   worktree, US-017 `GO`, US-018 v5 `GO`, and native-distribution-v2 `GO` on
   the real current native tree.
2. Record the pre-deletion commit and create a recovery branch or tag without
   deleting any existing branch.
3. Obtain any still-missing pinned-dependency and workflow approvals.
4. Update authorized workflows so no required job points at legacy paths.
5. Delete the 208 paths exactly as listed, one pathspec list, with no glob that
   can expand outside `packages/mcp-server`.
6. Apply the exact root manifest, lockfile, Knip, Turbo, Smithery, harness, and
   contributor-document edits listed above, including removal of the
   pre-deletion-only US-019 checker and root command.
7. Remove only the enumerated generated outputs.
8. Search the complete tracked tree for production imports and live
   documentation resolving through `distill-mcp` or `packages/mcp-server`.
9. Run native formatting, clippy, tests, corpus, and package smoke checks.
10. Recompute historical PRD and v1 through v5 evidence hashes. Any mismatch
    aborts delivery.
11. Commit the deletion as one reviewable slice. Do not publish, tag, bump a
    version, or merge.

If an unexpected import, required fixture, workflow, or supported behavior
depends on a deleted path, stop and restore before making another deletion
attempt.

## File-by-file rollback

Let `PRE_DELETE_COMMIT` be the clean commit recorded in step 2.

For each path in `legacy-deletion-files.txt`, restore exactly that file:

```bash
git restore --source="$PRE_DELETE_COMMIT" -- path/from/legacy-deletion-files.txt
```

Do not pass the workspace root or the package directory as a recursive restore
target. Iterate the reviewed file list so rollback has the same scope as
deletion.

Restore every modified or deleted root file from the same commit:

```text
package.json
bun.lock
knip.jsonc
turbo.json
smithery.yaml
CONTRIBUTING.md
AGENTS.md
CLAUDE.md
scripts/check-us019.mjs
```

Restore any separately authorized workflow file from the same commit. Then run
`bun install --frozen-lockfile`, the frozen legacy validation, and the native
validation. The rollback is complete only when the working tree and both
validation paths match the pre-deletion state.
