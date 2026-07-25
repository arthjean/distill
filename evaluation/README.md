# Projection evaluation assets

This directory freezes the language-neutral evidence used by the Distill
replacement:

- `corpus/manifest.jsonl`: 102 synthetic or scrubbed fixtures;
- `corpus/schema.json`: portable structural schema;
- `corpus/budget-profiles.json`: named byte and token budgets;
- `corpus/check.mjs`: deterministic generation, semantic validation, oracle
  self-tests, and secret scanning;
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

`--write` is deterministic for corpus artifacts. The legacy runner imports the
retired TypeScript source and is intentionally non-executable after its
deletion. Its committed evidence remains immutable.
