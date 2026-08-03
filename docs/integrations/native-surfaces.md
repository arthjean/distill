# Native product surfaces

The native `distill` binary exposes the same Rust `Engine` through three thin
surfaces:

- CLI: `project`, `artifact get`, `artifact trace`, `artifact slice`,
  `artifact search`, `status`, `gc`, `read`, and argv-only `run`.
- Codex: `codex-hook --mode off|observe|active`, installed into a versioned
  `PostToolUse` hook with `distill setup codex`.
- Claude Code: stdio MCP with exactly `distill_read`, `distill_run`,
  `distill_artifact_slice`, and `distill_artifact_search`, installed with
  `distill setup claude`.

Requests use `distill.context/v3`. Its only additions are the optional artifact
selector and the optional focus, so a `distill.context/v2` request that names
neither behaves exactly as before; one that names either is refused with
`schema_unsupported`.

Both product surfaces send `auto/v1` and declare no content class: a hook or an
MCP client cannot know what a command was about to print, so the engine derives
the policy from the shape of the observation it captured. The MCP tools publish
no preservation argument at all, and the CLI keeps `--profile` for the operator,
defaulting to the same `auto/v1`. Every retired profile identifier still
resolves, per the table in ADR-001. The versioned matrices pin the default, the
shape policies, the retired mapping, and the receipt fields that record which
policy ran.

Both setup targets require an explicit absolute configuration path, support
`--dry-run`, preserve an exact first-install backup, are idempotent, and restore
that backup with `--restore`. Codex setup rejects `--root`; Claude setup rejects
`--mode` and duplicate root IDs. These target-specific options fail before
configuration mutation.

Codex replacement uses documented `PostToolUse` blocking feedback because
`updatedMCPToolOutput` is parsed but unsupported. The adapter caps feedback at
2,250 tokens against Codex's approximate 2,500-token model-visible hook-output
limit. The current versioned matrix is
[`codex-hook-conformance-v2.json`](codex-hook-conformance-v2.json). The closed
v1 matrix remains unchanged. Hosted tools and specialized paths that do not
emit `PostToolUse` remain blind spots and
cannot produce a Distill diagnostic. One executable conformance test binds the
adapter input version, cap, supported modes, setup matcher, timeout, status
message, and generated command arguments to that matrix.
The hook receives `*` so unsupported events can produce a diagnostic, but the
adapter positively accepts only the versioned supported names and `mcp__`
family. Any other received tool name returns `unsupported_surface` without
capturing its response. Setup identifies an existing managed entry from its
complete hook structure, not from the display status alone, and refuses
ambiguous status collisions without changing the configuration.

Claude projection is explicit. `distill_read` and `distill_run` do not intercept
native Claude `Read` or `Bash`. MCP stdout contains JSON-RPC only, and
application failures use bounded `isError` tool results without raw source
bodies. An oversized 1 MiB MCP frame produces one protocol error, drains only
that frame when needed, and resumes at the next newline. The published
`distill_run` schema shares the engine's 4,096-argument, 1,048,576-byte
executable-plus-argv, and 100-through-300,000-ms timeout limits. Its `argv`
field is required by both the published schema and runtime decoder, including
when the literal argument vector is empty.

## Bounded recovery

Every surface that can return an omitting projection offers bounded retrieval on
that same surface, and no envelope advertises an unbounded recovery path. An
omitting envelope reports how much of the source it omitted, in the request's
count unit, and names the operation available where it is read: the two MCP
tools on Claude Code, and `distill artifact slice` or `distill artifact search`
on the Codex hook. An exact projection omits nothing and names nothing. An
encoded or metadata-only projection of a non-UTF-8 source reports its omission
without naming an operation, because selection is line and literal-text based.

`distill artifact get` is unchanged and remains the operator-facing full
recovery path outside the agent loop; it is published on no agent surface.

Retrieval selectors are bounded and literal. A pattern is at most 512 UTF-8
bytes, keeps at most 16 context lines on each side, and selects at most 32
matches; a value outside those bounds fails with `invalid_request` before any
store read, on both surfaces. Matching uses literal substring search: the binary
contains no regular-expression engine, so an agent-supplied pattern cannot
describe a catastrophic search. A pattern and a slice range are inert data and
are never evaluated as shell input, configuration, template, or instruction.

MCP retrieval arguments the decoder rejects produce a bounded `isError` tool
result naming the typed failure, rather than a JSON-RPC protocol error, so the
agent can correct the call from its own surface. The published retrieval schemas
are bound to [`mcp-conformance-v1.json`](mcp-conformance-v1.json).

## Stated intent

Every surface can say why an observation is being read. The four MCP tools each
publish an optional `focus` at the contract's 256-byte bound, the CLI accepts
`--focus TEXT` on every projecting command, and the Codex hook derives one from
the `tool_input` fields that state what a call was for: `command`, `file_path`,
`path`, `pattern`, and `query`. A payload field is deliberately not among them,
because a focus states the intent of a call rather than the body it wrote.

A declared focus outside the published bound is refused, exactly like an
oversized selector pattern, so the published schemas and the runtime decoder
agree. A focus the hook derives itself is truncated on a UTF-8 boundary instead,
because there the adapter is the author of the value; a tool input that is
absent, shaped differently, or empty simply yields no focus and the request
proceeds without one. A focus only orders which of the source's own lines the
budget keeps: it adds nothing, lifts no budget, and is never echoed into an
envelope, a receipt, or a diagnostic. The versioned matrices pin the field, its
bound, its optionality, and the receipt field that records that one applied.

CLI JSON uses `distill.cli/v2` and is bound to
[`cli-conformance-v3.json`](cli-conformance-v3.json), which supersedes the
closed [`cli-conformance-v2.json`](cli-conformance-v2.json) matrix and adds the
retrieval forms and selector bounds. Failure framing is selected
only by a parsed Distill `--json` option.
Values after the `run -- EXECUTABLE` delimiter remain literal child argv and
cannot select Distill output mode, and a `--pattern` value that looks like a
Distill option stays a literal pattern.
Configured-root membership is validated only by `Engine::handle`, so CLI and
MCP expose the same typed `unsafe_root` engine failure.

CLI status uses `distill.status/v2` and includes `lineage_bytes` plus
`max_lineage_bytes`. Garbage collection uses `distill.gc/v2` and separately
reports reclaimed source and lineage bytes. Artifact trace remains complete and
ordered within the 1 MiB per-artifact receipt limit.

No v1 support is claimed for Cursor, Windsurf, Continue, generic MCP clients,
Windows, macOS x86_64, Linux arm64, or Linux musl. The complete migration and
unsupported-surface matrix is
[`mcp-first-to-native.md`](../migration/mcp-first-to-native.md). The adapter
boundaries and embedded-native npm distribution decision are documented in
[`native-context-projection-engine.md`](../architecture/native-context-projection-engine.md).

Sources:

- [Codex hooks](https://learn.chatgpt.com/docs/hooks.md)
- [MCP architecture](https://modelcontextprotocol.io/specification/2025-06-18/architecture)
