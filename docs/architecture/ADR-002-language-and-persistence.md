# ADR-002: Projection engine language and persistence

- Status: Accepted
- Date: 2026-07-24
- Decision owner: Arthur Jean
- Depends on: [ADR-001](ADR-001-context-projection-contract.md)

## Context

Distill's replacement must persist untrusted observations before emitting an
omitting projection, recover exact bytes after restart, execute argv-only local
processes, and remain a small distributable binary. Familiarity or novelty is
not a sufficient language argument. EP-002 therefore implements the same
versioned JSONL protocol and evidence runner in Zig 0.16.0 and Rust 1.97.1.

The candidates share:

- `spikes/shared/protocol.schema.json` and the same versioned nine-fixture corpus
  subset;
- byte budgets and an explicit `token_profile_unsupported` response instead of
  an unversioned token heuristic;
- SQLite WAL, `synchronous=FULL`, a 250 ms default busy timeout, and source BLOB
  transactions verified by SHA-256 readback;
- bounded JSONL input, argv-only process execution, process-group cleanup,
  absolute deadlines, typed failures, and no runtime network path;
- release builds, two benchmark warm-ups, identical run counts, and one
  reference host.

The spikes are decision evidence, not the production interface. ADR-001 remains
authoritative. US-008 stays blocked until this ADR is accepted.

## Evidence

All measurements ran on host `dune`: Linux x86_64, kernel
`7.1.4-204.fc44.x86_64`, 16 logical CPUs, and SQLite 3.51.2. The protocol and
corpus subset hashes match across candidates.

| Knockout criterion                             |              Zig |             Rust |
| ---------------------------------------------- | ---------------: | ---------------: |
| Shared conformance                             |      PASS, 13/13 |      PASS, 13/13 |
| Runtime network dependency                     |             PASS |             PASS |
| Cold-start P95, limit 50 ms                    |   PASS, 0.979 ms |   PASS, 0.944 ms |
| 10 MiB peak RSS, limit 128 MiB                 | PASS, 72.566 MiB | PASS, 72.625 MiB |
| No crash over at least 3,600 child CPU-seconds | PASS, 3,602.88 s | PASS, 3,601.01 s |

The conformance suite covers invalid and oversized JSONL, the shared corpus,
determinism, exact recovery, token-profile rejection, argv handling, absolute
process deadlines, descendant cleanup, store limits, transaction ordering,
four interruption points, BLOB and database corruption, busy timeout, private
permissions, and linked runtime libraries.

| Release measurement     |       Zig |      Rust |
| ----------------------- | --------: | --------: |
| 1 MiB projection median | 57.623 ms | 52.965 ms |
| 1 MiB projection P95    | 59.986 ms | 58.001 ms |
| Stripped Linux binary   |   690 KiB |   815 KiB |
| Language package deps   |         0 |        34 |
| Cases per child CPU-s   |     34.15 |     67.35 |
| Validation cases        |   123,036 |   242,512 |
| Responses               |   123,036 |   242,512 |

Both candidates pass every knockout and the 100 ms 1 MiB projection target.
The longer validation is normalized by child CPU time, not wall time. Rust
processed about twice as many fixed-shape cases per CPU-second, while release
latency and memory were otherwise close. Zig's stripped artifact is 128,392
bytes smaller. This is a real distribution advantage, but not a runtime-memory
or latency advantage.

Evidence paths:

- `spikes/evidence/zig-conformance.json`
- `spikes/evidence/zig-benchmark.json`
- `spikes/evidence/zig-fuzz.json`
- `spikes/evidence/rust-conformance.json`
- `spikes/evidence/rust-benchmark.json`
- `spikes/evidence/rust-fuzz.json`

## Weighted decision

Eligible candidates are scored from 1 (materially weak) to 5 (strong).

| Axis                              |   Weight |      Zig |     Rust | Rationale                                                                                                                                                         |
| --------------------------------- | -------: | -------: | -------: | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Correctness and recoverability    |      30% |        5 |        5 | Both pass the same persistence, recovery, corruption, deadline, and interruption checks.                                                                          |
| Maintenance and memory safety     |      25% |        3 |        5 | Zig owns allocations and the SQLite C surface directly. Rust application code has one narrow `unsafe` process-group call and uses safe wrappers elsewhere.        |
| Ecosystem and adapter feasibility |      20% |        3 |        4 | Both support strict JSONL and local processes. Rust proved maintained serde and SQLite wrappers; exact tokenization and the MCP adapter remain unproven for both. |
| Performance                       |      15% |        4 |        5 | Both pass latency and RSS targets. Rust is slightly faster in the release benchmark and materially more CPU-efficient in the fixed validation workload.           |
| Distribution                      |      10% |        4 |        4 | Zig produced the smaller stripped artifact and integrates cross-compilation. Rust documents both v1 targets as Tier 1. Neither macOS artifact was executed.       |
| **Weighted total**                | **100%** | **3.85** | **4.70** |                                                                                                                                                                   |

The score is not based on implementation LOC. Rust wins on the costly parts of
the production path: ownership safety, maintained integration surfaces, and
CPU efficiency. Zig's 128,392-byte artifact-size advantage is not large enough
to change the product decision because cold-start latency and RSS are equal.

## Official documentation audit

The final audit compared the spike evidence with the official documentation for
the exact toolchains:

- [Zig 0.16 memory management](https://ziglang.org/documentation/0.16.0/#Memory)
  documents explicit allocators and no language runtime. This is a strong fit
  for bounded CLI work.
- [Zig build modes](https://ziglang.org/documentation/0.16.0/#Build-Mode)
  provide explicit `ReleaseFast`, `ReleaseSafe`, and `ReleaseSmall` choices.
  The measured spike used `ReleaseFast`; the size comparison strips both
  measured release artifacts without changing their code.
- [Zig 0.16 release notes](https://ziglang.org/download/0.16.0/release-notes.html#Roadmap)
  show substantial cross-compilation progress and still list completing and
  stabilizing the language as future roadmap work.
- [Rust ownership](https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html)
  provides memory management checked by the compiler without runtime overhead
  or a garbage collector.
- [Cargo release profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
  expose optimization, stripping, LTO, codegen-unit, and panic controls, so a
  Rust binary is not structurally prevented from being small.
- [Rust editions](https://doc.rust-lang.org/edition-guide/editions/)
  commit stable features to continued support while keeping migrations opt-in.
- [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html)
  lists Linux x86_64 and macOS arm64 as Tier 1 targets built and tested after
  each change.

Zig therefore wins two legitimate lightness dimensions: stripped artifact size
and build-graph simplicity. The Zig spike uses its standard library plus the
same system libraries; the Rust spike has seven direct and 34 total normal
dependency packages. Rust wins the dimensions that dominate Distill's total
cost: CPU efficiency in the shared workload, stable evolution, supported
release targets, and reusable persistence and serialization components. Both
remain native, no-GC deployment options.

## Required technical comparisons

| Area             | Zig                                                                   | Rust                                                                                   |
| ---------------- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Tokenizer        | Explicitly unsupported                                                | Explicitly unsupported                                                                 |
| SQLite           | Direct C API                                                          | `rusqlite` 0.40.1 over system `libsqlite3`                                             |
| Subprocess       | Zig process and I/O APIs, new process group, shared absolute deadline | Standard process APIs, `wait-timeout`, new process group, narrow `libc::kill` boundary |
| JSON/JSONL       | Typed `std.json`, bounded line reader                                 | Strict serde types, bounded line reader                                                |
| Sanitizers       | Not measured; CPU-accounted validation completed                      | Not measured; CPU-accounted validation completed                                       |
| MCP adapter cost | Not implemented; versioned JSONL seam is host-neutral                 | Not implemented; versioned JSONL seam is host-neutral                                  |
| Linux x86_64     | Release build and full evidence pass                                  | Release build and full evidence pass                                                   |
| macOS arm64      | Not executed                                                          | Not executed                                                                           |

No score assumes sanitizer or macOS evidence that does not exist. Rust's safety
advantage is based on the checked application boundary and wrapper surface, not
on an unrun sanitizer configuration. Exact, named tokenizer support remains a
production gate for US-011. macOS arm64 remains a qualification gate in US-018.

## Persistence decision

Select SQLite WAL through `rusqlite` 0.40.1, linked to the platform
`libsqlite3`, with `synchronous=FULL`, bounded busy timeouts, raw source BLOBs,
and SHA-256 readback before any omitting outcome is returned.

The production artifact module owns schema versions and migrations. Adapters do
not own SQL, transactions, retention, or recovery semantics. The spike does not
justify enabling `rusqlite`'s bundled SQLite feature because that packaging path
was not measured.

Reopen this decision before migration if any of these conditions occurs:

1. System SQLite cannot be packaged consistently for Linux x86_64 and macOS
   arm64.
2. The selected binding fails later eight-writer, retention, garbage collection,
   crash-recovery, or schema-migration gates.
3. A required exact tokenizer cannot be shipped without a runtime network path.
4. The production implementation requires a materially larger unsafe boundary
   or an unmaintained fork.

The reopened ADR must compare platform SQLite, bundled SQLite, Zig, and
`neither` against the failed gate. It must not silently change the binding.

## Decision

Select Rust, with Rust 1.97.1 as the initial pinned toolchain and `rusqlite`
0.40.1 as the initial SQLite binding. EP-002 and US-007 are complete. US-008 is
unblocked.

## Approval

- Proposed decision: Rust with SQLite WAL through `rusqlite` 0.40.1
- Approver: Arthur Jean
- Recorded approval: Explicitly confirmed by Arthur Jean on 2026-07-24; Rust is
  the frozen implementation language for Distill
