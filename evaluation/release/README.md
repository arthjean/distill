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
