#!/usr/bin/env bun

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const protocolPath = join(
  root,
  "evaluation/release/native-distribution-v1-protocol.json",
);
const protocol = json(protocolPath);
const validateOnly = process.argv.includes("--validate-protocol");
const externalRoot = option("--external-root");
const macosReceipt = option("--macos-receipt");

function option(name) {
  const index = process.argv.indexOf(name);
  return index === -1 ? null : process.argv[index + 1] ?? null;
}

function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function git(args) {
  return execFileSync("git", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function historicalIntegrity() {
  return JSON.parse(
    execFileSync(
      process.execPath,
      [join(root, "scripts/check-us019.mjs"), "--historical-only"],
      {
        cwd: root,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      },
    ),
  );
}

const coveragePath = join(root, protocol.coverage.evidence);
const workflowPath = join(root, protocol.macos.workflow);
const headRevision = git(["rev-parse", "HEAD"]);
const headNativeTree = git(["rev-parse", "HEAD:native/distill-core"]);
const historical = historicalIntegrity();
const protocolBlockers = [];

if (
  protocol.schema_version !== "distill.native-distribution-protocol/v1" ||
  protocol.qualification_id !== "native-distribution-v1-20260724" ||
  protocol.status !== "PREREGISTERED"
) {
  protocolBlockers.push("qualification protocol identity is invalid");
}
if (
  protocol.native_tree !== headNativeTree ||
  git(["status", "--porcelain=v1", "--untracked-files=all", "--", "native/distill-core"]) !==
    ""
) {
  protocolBlockers.push("HEAD/worktree native tree is not the preregistered tree");
}
if (
  sha256(coveragePath) !== protocol.coverage.sha256 ||
  json(coveragePath).status !== "PASS" ||
  json(coveragePath).native_tree !== headNativeTree ||
  json(coveragePath).after.lines.percent <
    protocol.coverage.required_line_percent ||
  json(coveragePath).after.branches.percent <
    protocol.coverage.required_branch_percent
) {
  protocolBlockers.push("native coverage prevalidation is invalid");
}
if (sha256(workflowPath) !== protocol.macos.workflow_sha256) {
  protocolBlockers.push("macOS workflow changed after preregistration");
}
if (
  protocol.coverage.full_native_check_executions !== 1 ||
  protocol.linux.executions !== 1 ||
  protocol.linux.package_repetitions !== 2 ||
  protocol.macos.executions !== 1 ||
  protocol.failure_policy.retry !== false
) {
  protocolBlockers.push("one-shot execution policy is invalid");
}
if (
  historical.status !== "VALID" ||
  historical.historical_prds !== 9 ||
  historical.historical_legacy_evidence !== 3 ||
  historical.historical_qualification_artifacts !== 34
) {
  protocolBlockers.push("historical integrity baseline is invalid");
}

if (protocolBlockers.length > 0) {
  throw new Error(protocolBlockers[0]);
}
if (validateOnly) {
  process.stdout.write(
    JSON.stringify(
      {
        status: "VALID",
        candidate_revision: headRevision,
        native_tree: headNativeTree,
        historical_prds: historical.historical_prds,
        historical_qualification_artifacts:
          historical.historical_qualification_artifacts,
      },
      null,
      2,
    ) + "\n",
  );
  process.exit(0);
}
if (externalRoot === null || macosReceipt === null) {
  throw new Error(
    "--external-root and --macos-receipt are required for final aggregation",
  );
}
if (resolve(externalRoot) !== resolve(protocol.linux.external_root)) {
  throw new Error("external Linux evidence root does not match the protocol");
}

const sources = {
  automated: join(externalRoot, basename(protocol.linux.evidence.automated)),
  suite: join(externalRoot, basename(protocol.linux.evidence.suite)),
  fuzz: join(externalRoot, basename(protocol.linux.evidence.fuzz)),
  package: join(externalRoot, basename(protocol.linux.evidence.package)),
  execution: join(externalRoot, basename(protocol.linux.evidence.execution)),
  macos: resolve(macosReceipt),
};
for (const [name, path] of Object.entries(sources)) {
  if (!existsSync(path)) {
    throw new Error(`missing ${name} evidence at ${path}`);
  }
}

const automated = json(sources.automated);
const suite = json(sources.suite);
const fuzz = json(sources.fuzz);
const packageEvidence = json(sources.package);
const execution = json(sources.execution);
const macos = json(sources.macos);
const candidate = execution.candidate_revision;
const candidateNativeTree = git([
  "rev-parse",
  `${candidate}:native/distill-core`,
]);
const attestationRef = protocol.candidate_binding.attestation_ref;
const remoteAttestation = git(["ls-remote", "origin", attestationRef]).split(
  /\s+/,
)[0];
const blockers = [];

if (
  execution.status !== "GO" ||
  execution.linux_executions !== 1 ||
  execution.package_repetitions !== 2 ||
  execution.full_native_check_executions !== 1
) {
  blockers.push("Linux one-shot execution receipt is not GO");
}
if (
  candidate !== headRevision ||
  execution.native_tree !== protocol.native_tree ||
  candidateNativeTree !== protocol.native_tree
) {
  blockers.push("candidate revision does not contain the preregistered tree");
}
if (
  automated.status !== "GO" ||
  automated.git_revision !== candidate ||
  automated.linux_release?.git_revision !== candidate ||
  automated.linux_release?.result !== "passed" ||
  automated.linux_release?.native_prevalidation?.native_tree !==
    protocol.native_tree ||
  automated.fuzz?.git_revision !== candidate ||
  automated.fuzz?.result !== "clean"
) {
  blockers.push("Linux release evidence is not GO on the candidate");
}
if (
  suite.result !== "passed" ||
  suite.git_revision !== candidate ||
  suite.native_prevalidation?.native_tree !== protocol.native_tree ||
  fuzz.result !== "clean" ||
  fuzz.git_revision !== candidate ||
  fuzz.cpu_seconds < 3600
) {
  blockers.push("Linux suite or fuzz evidence is invalid");
}
if (
  packageEvidence.status !== "GO" ||
  packageEvidence.candidate_revision !== candidate ||
  packageEvidence.native_tree !== protocol.native_tree ||
  packageEvidence.repetitions !== 2 ||
  packageEvidence.byte_identical !== true ||
  packageEvidence.checksum_sidecars?.independently_validated !== true ||
  packageEvidence.checksum_sidecars?.first_sha256 !==
    packageEvidence.checksum_sidecars?.second_sha256
) {
  blockers.push("Linux package reproducibility or checksum evidence is invalid");
}
if (
  macos.status !== "GO" ||
  macos.target !== "macos-arm64" ||
  macos.git_revision !== candidate ||
  macos.source_worktree_clean !== true ||
  !macos.workflow_run_url?.startsWith(
    "https://github.com/arthjean/distill/actions/runs/",
  )
) {
  blockers.push("macOS receipt is not GO on the candidate revision");
}
if (remoteAttestation !== candidate) {
  blockers.push("published attestation ref does not point to the candidate");
}

const destinations = {
  automated: join(root, protocol.linux.evidence.automated),
  suite: join(root, protocol.linux.evidence.suite),
  fuzz: join(root, protocol.linux.evidence.fuzz),
  package: join(root, protocol.linux.evidence.package),
  execution: join(root, protocol.linux.evidence.execution),
  macos: join(root, protocol.macos.evidence),
};
for (const path of Object.values(destinations)) {
  if (existsSync(path)) {
    throw new Error(`immutable evidence path already exists: ${path}`);
  }
  mkdirSync(dirname(path), { recursive: true });
}
for (const name of Object.keys(destinations)) {
  copyFileSync(sources[name], destinations[name]);
}

const aggregatePath = join(root, protocol.aggregate.evidence);
if (existsSync(aggregatePath)) {
  throw new Error(`immutable aggregate path already exists: ${aggregatePath}`);
}
const aggregate = {
  schema_version: "distill.native-distribution-qualification/v1",
  qualification_id: protocol.qualification_id,
  candidate_revision: candidate,
  native_tree: protocol.native_tree,
  coverage: {
    evidence: protocol.coverage.evidence,
    sha256: protocol.coverage.sha256,
    lines_before_percent: json(coveragePath).before.lines.percent,
    lines_after_percent: json(coveragePath).after.lines.percent,
    branches_before_percent: json(coveragePath).before.branches.percent,
    branches_after_percent: json(coveragePath).after.branches.percent,
    status: "PASS",
  },
  linux: {
    evidence: protocol.linux.evidence,
    hashes: Object.fromEntries(
      Object.entries(destinations)
        .filter(([name]) => name !== "macos")
        .map(([name, path]) => [name, sha256(path)]),
    ),
    status: automated.status,
  },
  macos: {
    evidence: protocol.macos.evidence,
    sha256: sha256(destinations.macos),
    workflow_run_url: macos.workflow_run_url,
    status: macos.status,
  },
  package_reproducibility: {
    archive_sha256: packageEvidence.archive_sha256,
    byte_identical: packageEvidence.byte_identical,
    checksum_sidecars_valid:
      packageEvidence.checksum_sidecars.independently_validated,
  },
  historical_integrity: {
    historical_prds: historical.historical_prds,
    historical_legacy_evidence: historical.historical_legacy_evidence,
    historical_qualification_artifacts:
      historical.historical_qualification_artifacts,
    status: historical.status,
  },
  attestation: {
    ref: attestationRef,
    head: remoteAttestation,
  },
  first_defect: blockers[0] ?? null,
  status: blockers.length === 0 ? "GO" : "NO-GO",
};
writeFileSync(aggregatePath, `${JSON.stringify(aggregate, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(aggregate, null, 2)}\n`);
if (blockers.length > 0) {
  process.exit(1);
}
