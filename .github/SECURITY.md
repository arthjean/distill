# Security Policy

## Supported versions

| Version | Supported |
| ------- | --------- |
| `0.1.0` | Yes       |

Security fixes land on `dev`. Reports must identify the package version,
source commit or native archive SHA-256 being tested.

## Reporting a vulnerability

**Please do not open a public issue for security reports.**

Email **arthur.jean@strivex.fr** with:

- a description of the issue and its impact,
- steps to reproduce (or a proof of concept),
- the source commit or native archive SHA-256, Rust version, OS, and affected
  surface (CLI, Codex hook, or Claude explicit MCP adapter).

You can expect an initial acknowledgement within 72 hours. Once a fix is ready,
disclosure and any release are coordinated with the reporter. Reports are
credited unless you prefer to stay anonymous.

## Scope

Distill captures untrusted local bytes and persists them before projection. The
areas most relevant to its threat model are:

- artifact commit atomicity, checksums, expiration, tombstones, permissions,
  concurrent writers, and store-cap enforcement;
- configured-root traversal, symlink replacement, file identity, embedded NUL,
  and binary handling;
- argv-only process acquisition, working-directory confinement, cleared
  environments, timeouts, signals, and output limits;
- projection budgets, mandatory fact preservation, receipt lineage, and
  recovery instructions;
- adapter schema validation, atomic setup backup and restore, and the boundary
  between supported and unsupported host events;
- raw source or secret leakage through logs, errors, protocol output, or
  generated evidence;
- any runtime outbound network request during capture, projection, retrieval,
  trace, or garbage collection.
