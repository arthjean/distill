# Release qualification

## Native surface contract v2

`docs/integrations/cli-conformance-v2.json` and
`docs/integrations/codex-hook-conformance-v2.json` freeze the CLI root-failure
semantics and the positively classified Codex tool surfaces introduced by
`distill.cli/v2` and `codex.post-tool-use/v2`. Their executable conformance
tests run with `cargo test` and bind each fixture to the implementation.

These matrices qualify the serialized surface changes only. They do not alter,
replace, or relabel closed release receipts, and they do not establish a native
distribution or publication `GO` verdict.

## Native npm v0.1.0

`architecture-hardening-v3-20260731` terminated `NO-GO` after both platform
vectors passed: the Linux gate updated the stale native version in
`fuzz/Cargo.lock`, leaving the worktree dirty and refusing aggregation. Its
no-retry rule remains intact.

`architecture-hardening-v4-20260731` reached an aggregate `GO`, but package
assembly terminated `NO-GO`: the final release-mode tests left
`target/release/distill` linked to a test harness, so the receipt digests did
not match the release executables restored by `package-native.sh`.

`architecture-hardening-v5-20260731` qualifies the same corrected `0.1.0`
root-crate source tree and exact release executables. It does not include the
npm metadata or POSIX launcher in its source identity.

`architecture-hardening-v6-20260731` extends that identity with
`npm/distill/` and a frozen package-surface probe. Both platform vectors verify
the package metadata, npm OS restriction, symlink resolution, exact native
selection, unsupported architecture rejection, and Linux GNU-libc rejection.
It preserves every v5 native gate and ends with the same locked release rebuild.
Both platform receipts and the aggregate are `GO` for candidate
`8084c132fd252bb2be429067a181e8e525fea7ee`, source tree
`952711e440754621080d45f8870c55c3b3ce17c3`. The macOS run is
`https://github.com/arthjean/distill/actions/runs/30620666910`.

Validate the preregistration without executing a gate:

```bash
bun evaluation/release/run-architecture-hardening-v6.mjs --validate-only
```

After creating the exact owner-private authorization record declared by the
protocol, execute Linux locally and macOS through
`.github/workflows/native-macos-qualification.yml`:

```bash
bun evaluation/release/run-architecture-hardening-v6.mjs --execute linux-x86_64
bun evaluation/release/run-architecture-hardening-v6.mjs --aggregate
```

The macOS workflow uploads its external receipt and native archive separately.
Copy the receipt into the protocol-declared macOS directory on the clean
aggregate host before aggregation. Publication is permitted only when the
aggregate is `GO` and `scripts/package-npm.sh` verifies both archive checksums,
receipt bindings, and embedded binary digests. Receipts remain external; do not
write them under `evaluation/release/evidence/`.

## Future npm trusted publishing

`.github/workflows/publish-npm.yml` is the token-free publication boundary for
versions after the `0.1.0` bootstrap. A manual dispatch names the exact package
version. Separate GitHub-hosted Linux x86_64 and macOS arm64 jobs execute the
preregistered protocol and package their native binaries. The publish job
receives only the qualified tarball and package record from a separate
no-privilege assembly job. That job restores both private receipts, aggregates
them fail-closed, runs `scripts/package-npm.sh`, and dry-runs the exact tarball.
Only the final five-minute job can obtain an npm OIDC credential.

The npm trusted publisher must match repository `arthjean/distill`, workflow
filename `publish-npm.yml`, environment `npm`, and allowed action
`npm publish`. The GitHub `npm` environment should require maintainer approval
and restrict deployment to the release branch or protected release tags. No npm
token belongs in GitHub secrets.

## Architecture-hardening preregistration

`architecture-hardening-v1-20260727` terminated `NO-GO` on Linux because
branch coverage was 69.73392461197339%, below the 75% floor. Its no-retry rule
prevented macOS execution and aggregation. Its protocol, ledger, and runner
remain immutable and are pinned by the successor ledger.

`architecture-hardening-v2-20260727` freezes the distinct root-crate candidate
at `source_tree` `aba9accd84896918e5dcbc0a7bc6f5bd2419564c`. The protocol is
`architecture-hardening-v2-protocol.json`; its immutable preregistration,
v1 reconciliation, and historical-integrity record is
`architecture-hardening-v2-ledger.json`. The candidate adds tests for existing
fail-closed branches without changing production behavior or qualification
gates. The protocol covers Request v2 and framing conformance, persisted-proof
mutation and semantic checks, lineage limits and eight writers, migration
faults, the process watchdog, projection behavior and performance, corpus
preservation, zero-network behavior, setup and restore, and per-platform
release-binary digests.

Preregistration validation is local and does not run qualification commands:

```bash
bun evaluation/release/run-architecture-hardening-v2.mjs --validate-only
```

Execution remains blocked until Arthur separately creates the owner-private
external authorization record at
`/tmp/distill-architecture-hardening-v2-20260727-authorization.json`. It binds
the clean source revision and tree, exact protocol, ledger, and runner hashes,
the 14,400 CPU-second ceiling, zero subscription calls, zero incremental
dollars, destinations, and permitted actions. The runner then accepts exactly
one platform command vector per external destination:

```bash
bun evaluation/release/run-architecture-hardening-v2.mjs --execute linux-x86_64
bun evaluation/release/run-architecture-hardening-v2.mjs --execute macos-arm64
bun evaluation/release/run-architecture-hardening-v2.mjs --aggregate
```

The default destination is
`/tmp/distill-architecture-hardening-v2-20260727`. A dirty worktree, changed
historical hash, mismatched authorization, wrong platform, or pre-existing
target directory refuses execution. Symlinked, non-owner, or non-private
authorization and output paths also fail closed. The runner never writes under
`evaluation/release/evidence/`. Linux and macOS receipts must bind the same
authorized revision and preregistered source tree; the aggregate is `GO` only
when both receipts and every gate are `GO`.

## Current root-crate gate

The root Cargo layout introduces `distill.release-suite/v2` and
`distill.release-gate/v2`. It identifies the production source through
`source_tree`, the Git object ID of the canonical `git ls-tree` listing for
`Cargo.toml`, `Cargo.lock`, `clippy.toml`, `rust-toolchain.toml`, `src/`,
`tests/`, `examples/`, and `fuzz/`.

The versioned native-distribution and US-018 files under `evaluation/release/`,
their ledgers and evidence, and `scripts/qualify-native-distribution.sh` remain
closed and byte-identical. They qualify only the previous `native/distill-core`
tree. The root-layout source requires a separately preregistered qualification
before any current distribution claim or publication.

`scripts/qualify-release.sh` is the non-interactive Linux x86_64 automated
release gate. It runs the complete native release suite, an additional
release-mode fuzz campaign with at least one CPU-hour of aggregate execution,
and the evaluator that writes outside the closed evidence tree by default under
`/tmp/distill-root-layout-release/`.

The macOS workflow emits `distill.macos-qualification/v2` evidence bound to the
same `source_tree` and uploads it as
`distill-macos-arm64-root-layout-v1`. Neither output is current qualification
evidence until a new protocol preregisters its exact destination and hashes.

The report is `GO` only when all of these conditions hold:

- at least 100 corpus fixtures preserve 100% of P0 and at least 95% of P1 facts;
- qualifying inputs achieve at least 50% median visible-token reduction;
- every artifact restores byte-exactly after restart and with eight writers;
- latency and peak-RSS limits pass on the recorded reference machine;
- no corpus budget overrun, invalid artifact reference, or supported Codex raw
  leak is observed;
- the Linux x86_64 release suite and the additional fuzz campaign pass.

The evaluator uses two warm-ups and 20 measured runs. P95 is the nearest-rank
percentile. Performance numbers include binary startup, input transfer,
artifact commit, projection, and protocol serialization. The report records the
exact Git revision, release and fuzzer binary digests, and reference-machine
details. The fuzz binary is built before timing so compilation cannot contribute
to the required CPU-hour.

US-018 remains a separate qualification gate. It requires approved model-backed
paired tasks and execution on macOS arm64; Linux evidence must not be relabeled
as second-platform proof.

`bun evaluation/release/run-paired.mjs` runs 20 raw/projected task pairs through
the user's existing Codex CLI and Claude Code subscriptions. Child processes
receive no OpenAI or Anthropic API key, use ephemeral tool-restricted sessions,
and record CLI versions, exact requested or reported models, host-controlled
sampling fields, token counts, failures, confidence bounds, and subscription
usage telemetry in `evidence/paired-tasks.json`.

The qualification preserves `evidence/paired-tasks-attempt-1.json` and
`evidence/paired-tasks-attempt-2.json`. Attempt 1 returned `NO-GO` because
free-form Markdown made exact response scoring ambiguous even though both
missing facts remained present in the projections. Attempt 2 kept the same
exact P0 facts and introduced a JSON `decisive_lines` array; it isolated a
salience failure in two `test-log/v1` tasks. Attempt 3 fixes their projection
budget at 24 tokens, the smallest satisfiable value that places the failing-test
identity directly beside its expected/actual assertion. The approved
qualification ceiling is three 40-invocation attempts and zero incremental
dollars; every attempt uses only the existing subscriptions.
`paired-attempt-ledger.json` hashes all three reports and closes the
qualification at 120 invocations. The evaluator refuses any replay without a
new approval.

For the closed v1 qualification,
`.github/workflows/native-macos-qualification.yml` ran the release contract and
corpus suite on the official `macos-15` arm64 image. It also exercised Codex
and Claude setup, idempotent repeat, restore/uninstall, projection, and
byte-exact artifact recovery. The historical `macos-arm64.json` artifact
remains that qualification's second-platform receipt. The current workflow's
root-layout artifact cannot be substituted into the frozen v1 checker.

After downloading that receipt,
`bun evaluation/release/check-us018.mjs` produces the aggregate US-018 verdict.
It requires paired and macOS `GO` reports from the same clean source revision;
passing either gate independently cannot unblock migration.

The closed v1 qualification and all of its evidence remain immutable. The
separately authorized `us018-v2-20260724` qualification is pre-registered in
`paired-tasks-v2.json` and `paired-qualification-v2-ledger.json`. It defines 50
task pairs, 100 subscription invocations, zero incremental dollars, ten
five-task categories, balanced providers and invocation order, task-specific
questions, strict JSON schemas, and exact source-line rubrics. A raw baseline
below 48/50 or a projected-minus-raw delta below -2 percentage points is
`NO-GO`.

Before any subscription call, build the release binary and run:

```bash
bun evaluation/release/run-paired-v2.mjs --validate-only
```

The one-shot execution command writes its report and invocation ledger only to
`/tmp/distill-us018-v2-20260724`, leaving the candidate worktree clean:

```bash
bun evaluation/release/run-paired-v2.mjs --execute
```

Before execution, commit the complete pre-registration, then consume it in one
dedicated child commit that changes only
`paired-qualification-v2-ledger.json`, records the pre-registration commit, and
sets `status` to `CONSUMED`. Push that clean consumption commit so it is the
candidate qualified by macOS and both model CLIs. The runner rejects any other
history shape.

The fixed state directory is a secondary one-shot lock. The durable anti-replay
record is the pushed consumption commit plus
`refs/distill/qualifications/us018-v2-20260724`. The runner atomically extends
that Git ref after every invocation with hashes of the prompt, observation, raw
CLI stdout/stderr, and parsed response. Provider failures count against the
ceiling and are scored as failures; no observation is retried. Authentication
preflight must report ChatGPT login for Codex and first-party Claude Max login
for Claude, after all API and cloud-provider variables are removed. Copy the
completed report and execution ledger into their distinct v2 evidence paths
only after the run terminates.

The macOS workflow remains unchanged. Download its receipt for the same
candidate SHA as `evidence/macos-arm64-v2.json`, preserving the historical
receipt. `bun evaluation/release/check-us018-v2.mjs` then verifies historical
hashes, manifest and execution-ledger hashes, exactly 100 ordered invocations,
both independent `GO` gates, and the same clean source revision.

The closed v2 qualification and its `NO-GO` evidence remain immutable. The
separately authorized `us018-v3-20260724` qualification is fully frozen in
`paired-tasks-v3.json` and `paired-qualification-v3-ledger.json`. It retains 50
task pairs and the original exact rubrics, caps execution at 100 subscription
calls with no retry, pins Claude to `claude-fable-5`, disables prompt
suggestions, requires Claude to report only the exact first-party pinned model,
and compares the exact reported model identity list within every raw/projected
pair. The runner permanently stops after both observations of the first pair
that diverges or reports an unexpected model. Its one-shot external directory
and Git attestation ref make that early stop non-replayable.

Before any subscription call, run:

```bash
bun evaluation/release/run-paired-v3.mjs --validate-only
```

Commit the complete pre-registration, then consume it in a sole child commit
that changes only `paired-qualification-v3-ledger.json`. Push that clean
candidate so the unchanged macOS arm64 workflow and the paired evaluation bind
the same SHA. Execute once:

```bash
bun evaluation/release/run-paired-v3.mjs --execute
```

Execution writes only under `/tmp/distill-us018-v3-20260724`. Publish
`refs/distill/qualifications/us018-v3-20260724`, then copy the completed paired
report, execution ledger, and matching macOS receipt to their distinct v3
evidence paths. `bun evaluation/release/check-us018-v3.mjs` is `GO` only when
the paired gate and the successfully downloaded macOS workflow artifact are
independently `GO` on that clean candidate. Criteria are frozen before
execution and are not relaxed after a result.

The terminal v3 `NO-GO` evidence remains immutable. It established that Claude
Code can report an auxiliary Haiku model symmetrically even when the requested
model is pinned to `claude-fable-5` and prompt suggestions are disabled. The
separately authorized `us018-v4-20260724` qualification therefore distinguishes
the required primary model from auxiliary telemetry without weakening pair
identity: every Claude condition must report the exact first-party Fable
identity, and the complete canonical model lists must be byte-for-byte equal
between raw and projected. Additional models pass only when they are reported
symmetrically.

V4 otherwise preserves the v3 protocol: 50 frozen task pairs, at most 100
subscription calls, zero incremental dollars, no API or fallback API, no retry,
external one-shot evidence, a new Git attestation ref, and permanent early stop
on the first complete-list divergence or missing required Fable identity.

```bash
bun evaluation/release/run-paired-v4.mjs --validate-only
bun evaluation/release/run-paired-v4.mjs --execute
bun evaluation/release/check-us018-v4.mjs
```

Pre-registration and consumption remain separate commits. The unchanged macOS
arm64 workflow must qualify the consumption SHA, and the checker independently
downloads that run's receipt before the aggregate can become `GO`.

The terminal v4 `NO-GO` evidence remains immutable. It proves that Claude Code
can report auxiliary Haiku usage asymmetrically even when both answer-producing
conditions explicitly request and report first-party `claude-fable-5`. That
host-internal telemetry is not controllable through the subscription CLI.

The separately authorized `us018-v5-20260724` qualification therefore freezes
the PRD's same-model requirement at the controllable response-model boundary:
both conditions must request `claude-fable-5` and report the exact first-party
Fable identity. Missing or divergent primary identity stops execution
permanently. Complete `modelUsage` lists, including auxiliary Haiku entries, are
still hashed, attested, and reported for every call, but auxiliary-list
differences are diagnostic and cannot independently fail or pass the gate.

V5 keeps every other v4 constraint unchanged: 50 task pairs, at most 100
subscription calls, zero incremental dollars, no API or fallback API, no retry,
safe mode, no tools, no session persistence, external evidence, a new
attestation ref, and the same clean candidate SHA for macOS.

```bash
bun evaluation/release/run-paired-v5.mjs --validate-only
bun evaluation/release/run-paired-v5.mjs --execute
bun evaluation/release/check-us018-v5.mjs
```
