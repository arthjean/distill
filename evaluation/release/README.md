# Release qualification

`scripts/qualify-release.sh` is the non-interactive Linux x86_64 automated
release gate. It runs the complete native release suite, an additional
release-mode fuzz campaign with at least one CPU-hour of aggregate execution,
and the evaluator that writes
`evidence/automated-linux-x86_64.json`.

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

`.github/workflows/native-macos-qualification.yml` runs the release contract and
corpus suite on the official `macos-15` arm64 image. It also exercises Codex and
Claude setup, idempotent repeat, restore/uninstall, projection, and byte-exact
artifact recovery. The uploaded `macos-arm64.json` artifact is the
second-platform qualification receipt.

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
