# ADR-001: Language-neutral context projection contract

- Status: Accepted
- Date: 2026-07-24
- Decision owners: Distill maintainers
- Scope: Product contract and central engine boundary

## Context

Distill currently exposes compression through tools owned by an agent host. That
surface cannot guarantee that an observation is reduced before the host adds it
to model context, and its optional recovery store ends with the server process.
The replacement must instead define one local operation whose result is bounded,
traceable, and recoverable for a declared lifetime.

Projection is intentionally not described as semantically lossless. Arbitrary
bytes do not have one context-independent meaning, and an extractive policy can
omit a fact that a later task needs. The product guarantee is narrower:

1. source bytes are durably captured before an omitting result is released;
2. model-visible output fits an explicit budget;
3. mandatory facts are never silently discarded to meet that budget;
4. every outcome explains its lineage and fidelity; and
5. committed source bytes are recoverable until the artifact expires.

## Decision

Distill is a local context projection engine. Its central module exposes one
operation:

```text
handle(Request) -> Outcome | Failure
```

The notation is conceptual. It does not prescribe an implementation language,
serialization library, transport, process model, or concurrency runtime.

The operation is deterministic for the same normalized request, source bytes,
policy version, projection version, and tokenizer version. Artifact identifiers,
capture timestamps, and implementation timing fields are the only permitted
nondeterministic receipt fields.

## Public contract

### Request

```text
Request {
  contract_version: Version
  request_id: OpaqueId
  source: Source
  budget: Budget
  preservation_profile: PreservationProfileId
  retention: Retention
  metadata: ScalarMap
}
```

`contract_version` selects the complete public semantics. Unknown major versions
fail with `schema_unsupported`. `request_id` is supplied by the caller for
idempotency and trace correlation. It does not become an artifact identifier.

`metadata` accepts only bounded scalar values from a versioned allowlist. Source
bodies, command output, environment blocks, and nested untrusted documents do not
belong in metadata.

### Source

`Source` is a closed discriminated union with four v1 variants:

```text
Source =
  | inline {
      bytes: ByteSequence
      media_type: MediaType?
    }
  | file {
      root_id: ConfiguredRootId
      relative_path: PathBytes
      binary_policy: BinaryPolicy
    }
  | process {
      executable: PathBytes
      argv: List<ArgumentBytes>
      cwd_root_id: ConfiguredRootId
      cwd_relative_path: PathBytes
      timeout_ms: Integer
      environment_profile: EnvironmentProfileId?
    }
  | artifact {
      artifact: ArtifactRef
    }
```

- `inline` captures the supplied byte sequence without requiring valid UTF-8.
- `file` acquires one file beneath a configured local root. A root is resolved
  from local engine configuration, not accepted as an arbitrary request path.
- `process` launches one executable with an argv vector. It never implies a
  command shell, string interpolation, or configuration evaluation.
- `artifact` reuses a previously committed, unexpired source artifact.

An adapter may expose any subset of these variants. It may not change the
semantics of a variant or fabricate unsupported acquisition metadata.

### Budget

```text
Budget {
  unit: bytes | tokens
  total_visible_limit: NonNegativeInteger
  reserved_envelope: NonNegativeInteger
  token_profile: TokenProfileId?
}

projection_payload_limit =
  total_visible_limit - reserved_envelope
```

`total_visible_limit` covers everything the model can see after an adapter adds
its required envelope. `reserved_envelope` is therefore subtracted before the
engine projects the payload. The engine must prove:

```text
projection_payload_count <= projection_payload_limit
projection_payload_count + reserved_envelope <= total_visible_limit
```

The two counts use the declared unit. A byte budget has no tokenizer. A token
budget names an immutable tokenizer identifier and version through
`token_profile`; unknown profiles fail with `token_profile_unsupported`.
Adapters must measure their actual envelope and may reserve more than they use.
They may not reserve less and truncate the engine output later.

`reserved_envelope > total_visible_limit` is invalid. Zero is a valid total
budget, but normally cannot contain mandatory projection content.

### Retention

```text
Retention {
  expires_at: Timestamp?
  ttl_seconds: PositiveInteger?
}
```

At most one field may be provided. If neither is provided, the local default is
seven days. The engine records the resolved `expires_at` in `ArtifactRef`.
Recovery is guaranteed only before that timestamp and only after artifact
integrity verification succeeds.

### Outcome

```text
Outcome {
  visible: VisiblePayload
  artifact: ArtifactRef
  receipt: Receipt
}

VisiblePayload {
  bytes: Utf8ByteSequence
  media_type: text/plain
}
```

`visible` is the complete projection payload and is always valid UTF-8. Binary or
malformed source bytes are represented by an explicit deterministic encoding or
description policy. An adapter envelope is not part of `visible`.

If source content already fits the payload limit without transformation,
`visible.bytes` is byte-identical to the source when the source is valid UTF-8,
and fidelity is `exact`. Artifact and receipt data remain separate metadata and
are not appended to the exact payload by the engine.

### ArtifactRef

```text
ArtifactRef {
  schema_version: Version
  id: OpaqueId
  source_sha256: LowerHex64
  source_bytes: NonNegativeInteger
  created_at: Timestamp
  expires_at: Timestamp
}
```

An artifact reference is valid only after the complete source transaction has
committed. The stable `id` addresses the source bytes and metadata. It is opaque
to callers and carries no executable path. The digest is always SHA-256 over the
original byte sequence.

An implementation may deduplicate storage, but deduplication cannot shorten the
retention promised by an existing reference or make identifiers predictable from
sensitive source content.

### Fidelity

```text
Fidelity = exact | extractive | encoded | metadata_only
```

- `exact`: visible bytes equal valid UTF-8 source bytes.
- `extractive`: visible bytes contain selected source spans plus deterministic
  structural labels.
- `encoded`: source bytes required a deterministic model-visible encoding.
- `metadata_only`: no source body is visible, but safe acquisition metadata is.

No fidelity value claims semantic equivalence.

### Receipt

```text
Receipt {
  schema_version: Version
  request_id: OpaqueId
  source_sha256: LowerHex64
  artifact: ArtifactRef
  projection_version: Version
  policy_version: Version
  token_profile: TokenProfileId?
  original_count: NonNegativeInteger
  visible_count: NonNegativeInteger
  count_unit: bytes | tokens
  fidelity: Fidelity
  retained_spans: List<ByteSpan>
  omitted_spans: List<ByteSpan>
  preservation: PreservationResult
  acquisition: AcquisitionReceipt
}
```

Spans are half-open byte offsets into the committed source. They are sorted,
non-overlapping within each list, and cover every source byte exactly once when
the policy returns `extractive`. `PreservationResult` records the applied profile
and identifiers of all mandatory facts. It does not copy sensitive fact bodies.

`AcquisitionReceipt` records source-variant metadata needed to distinguish
complete, partial, timed-out, signaled, or failed acquisition. Process acquisition
keeps stdout and stderr as ordered byte events and separately records exit code,
signal, timeout, working directory identifier, and truncation state.

### Failure

`Failure` is a closed discriminated union. Every member contains `code`, a safe
message, `request_id` when decoding reached it, and bounded structured details.
It never contains raw source bodies.

```text
Failure =
  | invalid_request
  | schema_unsupported
  | source_unsupported
  | token_profile_unsupported
  | budget_unsatisfiable
  | input_too_large
  | resource_exhausted
  | unsafe_root
  | permission_denied
  | acquisition_failed
  | store_full
  | store_busy
  | commit_failed
  | artifact_unknown
  | artifact_expired
  | artifact_corrupt
  | artifact_schema_unsupported
  | invariant_breach
```

Only failures after a successful source commit may contain `artifact`. The
reference is required for `budget_unsatisfiable`: if mandatory preserved facts
alone exceed the payload limit, the engine commits the complete source, returns
`budget_unsatisfiable` with that reference, emits no partial projection, and
never omits a mandatory fact silently.

Acquisition failures may include a safe partial-acquisition receipt, but partial
bytes are not exposed as a complete projection. An invariant failure emits no
reduced visible content and no falsely valid artifact reference.

## Preservation contract

A preservation profile is versioned evidence, not an open-ended prompt. It
identifies mandatory P0 fact rules and measured P1 fact rules for a content
class. Projection may reduce only after all applicable P0 facts are located.

When a policy cannot determine whether a mandatory fact survives, it fails
closed with `budget_unsatisfiable` or `invariant_breach`; it does not substitute
a generative summary. P1 recall is measured and reported but does not override a
P0 or budget guarantee.

## Commit and release ordering

For every result that can omit source bytes:

```text
acquire bounded source
  -> validate storage root and permissions
  -> atomically commit source bytes and metadata
  -> verify committed artifact identity
  -> derive projection
  -> validate preservation and budget
  -> release Outcome or post-commit Failure
```

No projection, receipt, or artifact reference crosses the central boundary
before the source commit succeeds. A failed permission check, unsafe root,
failed atomic commit, or corrupt readback therefore cannot produce an omitting
projection.

## Adapter modes

Modes are adapter rollout policy and do not enter the central engine types:

| Mode | Engine invocation | Content exposed to the host |
|---|---|---|
| `off` | None | The adapter may pass through the host's raw result. Distill provides no capture or recovery guarantee. |
| `observe` | Shadow invocation only | The adapter passes through raw content and records safe comparison metrics. A shadow projection is never substituted into model-visible output. |
| `active` | Authoritative invocation | Only the bounded projection is exposed, except that an `exact` outcome naturally contains the unchanged source. Failures follow the adapter's explicit fail-closed policy and never silently fall back to raw content. |

Because `off` and `observe` can expose raw content, their configuration must say
so. `active` may not expose raw source as an undocumented fallback.

## Module boundary

The central module owns acquisition semantics, artifact ordering, projection,
receipts, recovery, and typed failure semantics. It accepts only the types above
plus private runtime ports for filesystem, clock, process, randomness, and
artifact storage.

Transport schemas, agent event objects, remote procedure envelopes, lifecycle
callbacks, and vendor SDK types are adapter concerns. They cannot appear in the
central public contract or its dependency graph.

## Non-goals

The following legacy behavior is deliberately outside this decision:

- preserving an invariant that the product exposes exactly three tools;
- retaining sandboxed QuickJS execution as a projection feature;
- generative or model-authored summarization;
- claiming universal interception of native Claude operations;
- treating process-scoped recovery as durable recovery;
- maintaining wire compatibility with the current MCP-first surface; and
- choosing Zig, Rust, SQLite bindings, tokenizer libraries, or adapter SDKs.

## Consequences

The engine can be evaluated without an agent host or transport. Adapters become
translation and injection layers, not alternate policy implementations.
Persistence is mandatory even when a projection seems easy, so active operation
has unavoidable local I/O cost. Exact small content also has an artifact because
lineage and later recovery are product properties, not compression side effects.

The language spikes must implement this contract rather than define it. If both
candidate implementations expose different observable behavior, the candidate
is non-conformant rather than evidence that the contract should silently drift.
