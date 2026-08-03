# Projection evaluation assets

This directory freezes the language-neutral evidence used by the Distill
replacement:

- `corpus/manifest.jsonl`: 102 synthetic or scrubbed fixtures;
- `corpus/schema.json`: portable structural schema;
- `corpus/budget-profiles.json`: named byte and token budgets;
- `corpus/check.mjs`: deterministic generation, semantic validation, oracle
  self-tests, and secret scanning;
- `corpus/real/`: 48 captured real tool-output fixtures with their manifest and
  capture runner;
- `baseline/projection-baseline-v1.json`: frozen executed-path projection
  behavior over the real corpus;
- `legacy/evidence.json`: raw measurements against the retired TypeScript
  implementation;
- `legacy/baseline.md`: human-readable baseline and salvage matrix; and
- `legacy/run.mjs`: archival measurement recipe retained with the closed
  evidence.

Historical PRDs and current production files are inputs only. EP-001 does not
modify them.

## Corpus contract

Each JSONL record contains:

```text
schema_version
id
category
description
reducible
source
source_sha256
budget_profile
expected
annotations.p0[]
annotations.p1[]
```

Payloads use one of three portable encodings:

- `utf8`: the JSON string encoded as UTF-8;
- `base64`: arbitrary bytes; or
- `padded`: a base64 prefix followed by one repeated byte to an exact length.

For a process source, canonical source bytes are event payloads concatenated in
ascending event order. Stream boundaries, stdout, stderr, exit status, signal,
timeout, working directory, and truncation state remain separately validated
metadata. A source digest is SHA-256 over the canonical source bytes.

Facts are byte needles encoded as base64. A fact is recalled only when the exact
needle occurs in model-visible bytes. No Unicode normalization, stemming,
semantic matching, or model judgment is allowed. For a fixture:

```text
P0 recall = recalled P0 facts / declared P0 facts
P1 recall = recalled P1 facts / declared P1 facts
```

An empty fact class has recall 1.0 by convention. A reducible fixture must
declare at least one P0 and one P1 fact, and every declared needle must occur in
the canonical source. This makes scoring identical across implementation
languages.

Byte reduction is computed directly from byte lengths. Token reduction is valid
only when the runner uses the exact tokenizer ID and version named by the budget
profile. Byte counts are never relabeled as tokens. Invalid, crashed,
nondeterministic, or incomplete runs retain raw evidence but do not contribute
to aggregate recall, latency, or reduction.

## Categories and boundaries

The corpus covers build output, test output, logs, diffs, diagnostics, stack
traces, source code, JSON, empty output, Unicode, malformed bytes, and
prompt-injection-shaped content. Process fixtures retain stdout and stderr event
order.

Dedicated boundaries cover zero bytes, one byte, an exact 224-byte projection
payload budget, one byte over that budget, 1 MiB, and 10 MiB.

All paths, identities, diagnostics, and payloads are synthetic. The committed
scan rejects common private-key, cloud credential, access-token, and
secret-assignment shapes. This scan is a repository hygiene gate, not a product
claim that Distill automatically detects secrets or PII.

## Commands

From the repository root:

```bash
bun evaluation/corpus/check.mjs --write
bun evaluation/corpus/check.mjs --verify
```

## Real tool-output corpus

`corpus/real/` holds output captured from real developer tools, not generated
text. It exists alongside the generated corpus, which stays unchanged as a
regression fixture. 48 fixtures cover eight shapes, six each: `build-output`,
`test-output`, `typecheck-lint`, `stack-trace`, `unified-diff`, `api-json`,
`source-file`, and `terminal-log`.

Every fixture is the exact output of one executable invoked with literal argv,
with stdout followed by stderr. `manifest.jsonl` records, per fixture, the
`distill.real-corpus/v1` schema version, its identifier, shape label,
description, originating command, working-directory class, capture host class,
exit code, per-stream byte counts, stored byte length, line count, UTF-8
validity, truncation state and pre-truncation length, SHA-256, and stored path.
Fixtures are capped at 256 KiB and truncated on a line boundary.

Capture normalizes host identity before digesting: the work directory,
repository root, and home directory become `/home/dev/work`, `/home/dev/distill`,
and `/home/dev`; every address becomes `dev@example.invalid`; the commit author
name becomes `Example Developer`; and the account name becomes `dev`. A scrub
scan then rejects private keys, cloud credentials, access tokens, secret
assignments, any other home path, and any other address. `capture_host_class` is
a platform and architecture class such as `linux-x86_64`, never a host name.

Capture is not deterministic and is not re-run by the gate. The manifest is the
frozen record, and `--verify` proves every stored fixture still matches its
declared length, digest, encoding, and scrub state.

```bash
bun evaluation/corpus/real/capture.mjs          # first capture
bun evaluation/corpus/real/capture.mjs --force  # replace a committed capture
```

Capture resolves every executable first and fails closed with the missing
command named, before any staging directory, fixture, or manifest byte is
written and before the overwrite guard. `check.mjs` proves that end to end by
running the capture runner with an empty `PATH` and asserting the manifest stays
byte-identical with no staging directory left behind. Repository-scoped git
fixtures name explicit revisions so their commands stay reproducible from any
clone. The `json-distill-status` fixture requires `target/release/distill`.

## Frozen executed-path baseline

`baseline/projection-baseline-v1.json` records what the shipped projection
actually does, measured through `distill project --json` with
`plain-text/v1`, the profile both product surfaces pinned when the baseline was
frozen. EP-004 moved the executed default to `auto/v1`, which derives the policy
from the shape of the observation; `plain-text/v1` still resolves, to the
line-structured policy, so the recorded run stays reproducible. It covers every real
corpus fixture at four budgets: the executed Codex hook default (2250 total,
450 reserved), the corpus token profile (512/64), a large host budget
(8192/1024), and the zero-payload boundary (450/450). Each result records
original count, visible count, budget utilization, retained byte ratio, retained
bytes, retained and omitted span counts, and fidelity. A typed failure is
recorded with its code instead of aborting the run: the zero-payload budget
yields 48 `budget_unsatisfiable` results.

The evidence binds the source revision, release binary digest, and real corpus
manifest digest. Writing over it requires `--force`; it is frozen comparison
evidence, not a receipt to refresh.

```bash
cargo build --locked --release
bun evaluation/baseline/run-projection-baseline.mjs
```

The run fails closed unless it reproduces the two measurements recorded in the
projection-intelligence PRD within one percentage point: 0.39% payload
utilization on `src/artifact.rs` at 2799 tokens, and 2.4% on `git log --stat
-40`. The command log counts 5821 tokens rather than 6341 because the stored
fixture is scrubbed; the utilization it reproduces is 2.44%. At the executed
default, median utilization across the 14 over-budget fixtures is 0.78%, while
the maximum is 100% on single-line input, where the prefix fallback is the only
path that spends the budget.

`tests/contract_foundation.rs` asserts the same accounting through the public
seam: `projection_matrix_reports_budget_utilization_and_retention` freezes
visible count, utilization, retained ratio, and span counts for an over-budget
line-structured source, and applies exact fidelity instead of the floor when a
source already fits. `over_budget_projection_must_spend_its_payload_budget`
asserts the 80% payload-budget floor and is marked as the expected failure this
baseline records.

### Rust corpus consolidation decision

EP-001 US-004 records `NO-GO` for extracting corpus semantics into a shared
dev-only Rust module. The root crate currently has one Rust corpus consumer,
`tests/corpus.rs`; release evaluators are JavaScript protocols, and the closed
v1-v5 evaluators cannot be rewritten. A new Rust crate or production dependency
would therefore create an abstraction for one consumer and would not make the
JavaScript evaluator share its implementation.

The JavaScript validator remains authoritative for complete corpus validation
and deterministic materialization. The Rust integration test independently
implements only the semantics it consumes: strict fixture shape, source
materialization, category-to-profile mapping, digest and fact validation, and
byte-exact P0/P1 scoring. Both consumers run the same focused rejection classes:
noncanonical base64, digest mismatch, missing fact IDs, unknown keys, and
unknown categories. Unknown categories return explicit errors; neither consumer
falls back to plain text.

`--write` is deterministic for corpus artifacts. The legacy runner imports the
retired TypeScript source and is intentionally non-executable after its
deletion. Its committed evidence remains immutable.
