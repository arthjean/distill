# Release qualification

## Executed-path projection baseline

`evaluation/baseline/projection-baseline-v1.json` freezes what the shipped
`plain-text/v1` path does on the real tool-output corpus, at four budgets
including the executed Codex hook default. It is the comparison reference for
the projection-intelligence release and is created outside
`evaluation/release/evidence/`, which stays closed. It is not a qualification
verdict: it establishes ground truth before any policy changes, and it
overwrites and relabels no receipt. `evaluation/README.md` documents the corpus,
the capture rules, and the reproduction commands.

## Executed-path qualification

`executed-path-v1-protocol.json` preregisters the first qualification of the
projection path both product surfaces actually run, against the projection they
actually ran before this release. Every earlier paired protocol selected a
preservation profile per corpus category, which the product never does. Neither
arm here passes `--profile` at all: each binary runs the default its own tree
pins, so the measurement compares two executed paths. The runner aborts if
either receipt reports any other requested profile.

The comparator is the point of the design. The baseline arm is not a
reimplementation and not a deterministic footnote: it is the binary that
`evaluation/baseline/projection-baseline-v1.json` pins, rebuilt from revision
`5f228cd` and verified against the digest that receipt recorded, running its own
`plain-text/v1` default with no focus. Before any invocation the runner checks
that it reproduces the frozen receipt fixture by fixture. Note that
`--profile plain-text/v1` on the candidate binary is not a shortcut to this arm:
the v3 contract keeps the retired identifier accepted but resolves it to the
terminal-log shape policy, which fills the budget.

The unbounded raw observation is deliberately not scored and consumes no
invocation. The closed v5 qualification measured it at 50 of 50 against 50 of
50 with a 0-point delta, and against the candidate's 25 of 26 retention ceiling
it can only tie or lose, so it cannot discriminate. No claim resting on this
evidence may assert parity with unbounded context.

The protocol pins the real corpus manifest digest, the production `source_tree`,
both binary digests, both executed default profiles, the model-scored budget
(the executed Codex hook default, 2250 visible tokens with a 450-token reserved
envelope) applied identically to both arms, the four deterministic accounting
budgets the frozen baseline recorded, the sample, and the gates.

The sample is 26 pairs and 52 subscription invocations: two exact-line questions
on each real corpus fixture that is over budget at the executed budget and
carries line structure, with 13 pairs in each invocation order.
`json-cargo-metadata-full` is excluded and recorded as such:
it is single-line minified JSON, so it carries no line rubric, and it stays in
the deterministic accounting only. The rubric is the closed v5 rule, unchanged:
strict JSON parse, exactly the declared keys, exact equality against one
complete source line that occurs exactly once in the raw observation.

This qualification runs one model host. The ChatGPT subscription is lapsed at
preregistration, so every pair goes through Claude Code at the pinned
`claude-fable-5` identity. The closed v5 protocol balanced two providers so that
one host's parsing or formatting quirks could not drive the result, and that
control is absent here; the invocation-order counterbalance is retained. It is
materially less damaging under this comparator, because a quirk would have to
discriminate between two payloads drawn from the same model on the same
question. Every claim resting on this evidence is scoped to that host, and
adding a second-host arm requires a new preregistration rather than an
amendment.

The aggregate is `GO` only when the paired report is `GO`, every preregistered
gate command passes, and the report still binds this protocol, source tree, and
both binaries. The paired report is `GO` only when all of these hold:

- projected answer accuracy beats baseline answer accuracy by at least 65
  percentage points;
- the projected condition answers at least 15 of 26, a breakage guard and never
  a performance gate;
- the baseline condition answers at most 8 of 26, above which the rubric is
  answerable without the observation often enough that the sample stops
  measuring retention;
- median budget utilization over the over-budget fixtures is at least 85%;
- deterministic answer-line retention over the sample is 25 of 26 for the
  candidate and 2 of 26 for the control;
- every pair reports the required primary model identity.

Each gate has one job, and the accuracy gate carries the claim alone. The
structural margin is 88.46 points; the 65-point floor absorbs two confounds
that the protocol records and the report measures. Model error costs the
candidate only. Prior-answerability costs the control only, because several
expected lines are short and idiomatic enough to be produced from priors, so it
deflates the delta rather than flattering it. The control condition is itself a
closed-book control on the 24 tasks whose answer line it does not retain, so
that count is published rather than treated as an anomaly, and no task is ever
excluded after the fact. The runner refuses any protocol whose worst admissible
case under the two guards already clears the delta floor, which is what keeps
the floor from being a restatement of the guards.

Both retention numbers are reproduced before execution, and the runner refuses
rather than adjusting a gate. `ep006-24` is retained by neither arm, so it costs
the candidate nothing and no `GO` requires the model host to fail a task. The
two control retentions, `ep006-19` and `ep006-21`, are the first line of a
`git show` and of a `git log -p`, which the prefix behavior of `plain-text/v1`
preserves. The runner also refuses if any task is retained by the control and
not by the candidate, which the delta floor would otherwise absorb silently.

Validate the preregistration without consuming an invocation. It rebuilds the
canonical binary, materializes the control binary from the pinned revision with
`git archive` into `/tmp/distill-executed-path-control-5f228cda`, so no worktree
is registered and the candidate checkout is untouched, verifies every pinned
digest, reproduces the preregistration measurement, and writes no receipt:

```bash
bun evaluation/release/run-executed-path-v1.mjs --validate-only
```

Execution is refused until the authorization ledger is durably consumed. Commit
the complete preregistration, then consume it in one dedicated child commit that
changes only `executed-path-v1-ledger.json`, records the preregistration commit,
and sets `status` to `CONSUMED`. The runner rejects any other history shape, and
refuses before reading a fixture or spawning a provider when the ledger is still
`PRE_REGISTERED`, so an unauthorized attempt records no partial result.

```bash
bun evaluation/release/run-executed-path-v1.mjs --execute
bun evaluation/release/check-executed-path-v1.mjs
```

Both write their report, execution ledger, and aggregate only under
`/tmp/distill-executed-path-v1-20260804`, never into
`evaluation/release/evidence/` and never into the worktree. The single durable
repository record is `refs/distill/qualifications/executed-path-v1-20260804`:
the runner creates it before the first invocation and atomically extends it
after every call with the hashes of the prompt, observation, raw CLI output,
and parsed response. The fixed state directory is the secondary one-shot lock;
that ref and the consumption commit are the durable anti-replay record.

The checker runs the preregistered contract, integrity, zero-network,
executed-path, and corpus gates before it looks for the paired report, and
writes no receipt when a gate fails or the report is absent.
The closed v5 protocol, ledgers, runner, checker, and evidence, and the frozen
projection baseline, are pinned by digest in the new ledger and verified on
every invocation.

### Executed-path v1, terminal

`executed-path-v1-20260804` terminated `NO-GO` after 10 of 52 invocations, on
`ep006-05`, reason `REQUIRED_PRIMARY_MODEL_NOT_REPORTED`. Claude Code answered
that pair's baseline condition with `claude-opus-5` and `claude-haiku-4-5` while
`--model` requested `claude-fable-5`, reporting `is_error` false,
`api_error_status` null and `terminal_reason` completed. The substitution was
silent and the nine other invocations reported the requested identity. Its
no-retry and no-replay rules stand: it is never rerun and never relabelled.

Its terminal evidence is preserved as
`executed-path-v1-terminal-report.json` and
`executed-path-v1-terminal-execution-ledger.json`, outside the closed
`evidence/` tree, and pinned by digest in the v2 ledger along with its protocol,
ledger, runner and checker. The five completed pairs measured baseline 0 of 5
against projected 5 of 5, with prior-answerability 0 and no model error on
retained content. That is diagnostic only: the sample is 5 of 26, no
preregistered gate was reached, and no release claim rests on it.

## Executed-path qualification v2

`executed-path-v2-protocol.json` supersedes v1 and changes three things. The
corpus, the binaries, the comparator, the 26 tasks, the rubric, the budgets and
every accuracy gate are byte-identical.

The requested model becomes `claude-opus-5`. Pinning a model no caller runs
reproduces the exact error this release exists to correct: measuring a
configuration the product does not meet.

No model identity is enforced in flight. Claude Code dispatches auxiliary and
subagent models of its own accord, and fighting the host is not a product
control. Every invocation still records and attests its complete `modelUsage`
list.

Internal validity moves to an end-of-run balance gate. For each reported model,
the asymmetry is the difference between how many baseline and how many projected
invocations used it; the statistic is the worst case over all models, and the
ceiling is 4 invocations. An asymmetry of k can inflate the delta by at most
k/26 of the sample, so 4 leaves 73.1 points against the 65-point floor. A host
that dispatches symmetrically scores zero. The rationale for moving rather than
deleting the control: v3, v4 and v1 each died near the tenth invocation on host
telemetry, so the criterion was sound and the instrument was not.

The one remaining in-flight terminal condition is three consecutive pairs of
provider errors, which stops a dead transport from consuming the ceiling.

```bash
bun evaluation/release/run-executed-path-v2.mjs --validate-only
bun evaluation/release/run-executed-path-v2.mjs --execute
bun evaluation/release/check-executed-path-v2.mjs
```

Preregistration and consumption remain two separate commits, in that order, and
execution writes only under `/tmp/distill-executed-path-v2-20260804` with its
durable record at `refs/distill/qualifications/executed-path-v2-20260804`.

### Result

`GO`. The complete sample ran: 52 invocations of 52, no early termination, no
provider error. The preregistration was committed at `d774056` and consumed at
`3351268`, both pushed before execution, so every gate was fixed before any
result existed.

| | control `plain-text/v1` | candidate `auto/v1` with focus |
|---|---|---|
| Exact answers | 2 of 26 | 25 of 26 |
| Budget utilization | 0.78% | 100% |
| Retained byte ratio | 0.15% | 36.3% |

The delta is 88.46 points against a 65-point floor, with a conservative 95%
Wilson interval of +57.0 to +97.2 points. Both confounds measured zero: prior
answerability is 0 over the 24 closed-book tasks, so the control's two successes
are exactly its two retained lines, and model error on retained content is 0.
The candidate's only failure is `ep006-24`, disclosed before execution as
retained by neither arm.

The balance gate that replaced the in-flight identity control reports an
asymmetry of 0 against a ceiling of 4: `claude-opus-5` on all 26 invocations of
each arm and the auxiliary Haiku on 22 of each. All 26 pairs reported the
requested model.

Receipts are preserved as `executed-path-v2-report.json`,
`executed-path-v2-execution-ledger.json` and `executed-path-v2-aggregate.json`,
outside the closed `evidence/` tree, relabelling and overwriting nothing.

The claim this evidence supports, and no more: at the budget the Codex hook
applies, on this real corpus, with Claude Code at `claude-opus-5`, the executed
`auto/v1` path answers questions the executed `plain-text/v1` path it replaces
could not. It says nothing about parity with unbounded context, which was
deliberately not measured, and it is scoped to one model host.

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
