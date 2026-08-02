#!/usr/bin/env bun

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const [protocolArgument, revision, mode] = process.argv.slice(2);

function fail(message) {
  throw new Error(message);
}

function readJson(fileName) {
  return JSON.parse(readFileSync(fileName, "utf8"));
}

function sha256(fileName) {
  return createHash("sha256").update(readFileSync(fileName)).digest("hex");
}

if (
  !protocolArgument ||
  !/^[0-9a-f]{40}$/.test(revision ?? "") ||
  (mode !== undefined && mode !== "--validate-only")
) {
  fail(
    "usage: create-architecture-authorization.mjs <protocol> <revision> [--validate-only]",
  );
}

const protocolPath = isAbsolute(protocolArgument)
  ? protocolArgument
  : join(root, protocolArgument);
const protocol = readJson(protocolPath);
const ledgerPath = join(root, protocol.tooling?.ledger?.path ?? "");
const runnerPath = join(root, protocol.tooling?.runner?.path ?? "");
const authorizationPath = protocol.authorization?.external_record;

if (
  protocol.schema_version !==
    "distill.architecture-hardening-qualification/v1" ||
  protocol.status !== "PREREGISTERED" ||
  typeof authorizationPath !== "string" ||
  !authorizationPath.startsWith("/tmp/") ||
  protocol.destinations?.default_root?.startsWith("/tmp/") !== true
) {
  fail("qualification protocol cannot authorize release execution");
}

const authorization = {
  schema_version: "distill.architecture-hardening-authorization/v1",
  qualification_id: protocol.qualification_id,
  authorized_by: "Arthur Jean",
  authorized_at: new Date().toISOString(),
  source_revision: revision,
  source_tree: protocol.source.source_tree,
  protocol_schema_version: protocol.schema_version,
  protocol_sha256: sha256(protocolPath),
  ledger_sha256: sha256(ledgerPath),
  runner_sha256: sha256(runnerPath),
  maximum_cpu_seconds: protocol.limits.maximum_cpu_seconds,
  maximum_subscription_invocations:
    protocol.limits.maximum_subscription_invocations,
  incremental_dollar_ceiling_usd:
    protocol.limits.incremental_dollar_ceiling_usd,
  destinations: protocol.destinations,
  permitted_external_actions: protocol.permitted_external_actions,
};

if (mode === "--validate-only") {
  process.stdout.write(`${authorizationPath}: valid\n`);
} else {
  writeFileSync(
    authorizationPath,
    `${JSON.stringify(authorization, null, 2)}\n`,
    { flag: "wx", mode: 0o600 },
  );
  process.stdout.write(`${authorizationPath}\n`);
}
