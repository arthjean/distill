#!/usr/bin/env bun

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  existsSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../..", import.meta.url));
const protocolPath = join(
  root,
  "evaluation/release/native-distribution-v2-protocol.json",
);
const protocol = json(protocolPath);
const validateOnly = process.argv.includes("--validate-only");
const unknownArguments = process.argv
  .slice(2)
  .filter((argument) => argument !== "--validate-only");

if (unknownArguments.length > 0) {
  throw new Error(`unknown argument: ${unknownArguments[0]}`);
}
if (
  protocol.schema_version !== "distill.native-distribution-protocol/v2" ||
  protocol.qualification_id !== "native-distribution-v2-20260725" ||
  protocol.status !== "PREREGISTERED" ||
  protocol.aggregate.evidence !==
    "evaluation/release/evidence/native-distribution-v2.json"
) {
  throw new Error("qualification protocol identity is invalid");
}

function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

function sha256(pathOrBytes) {
  const bytes = Buffer.isBuffer(pathOrBytes)
    ? pathOrBytes
    : readFileSync(pathOrBytes);
  return createHash("sha256").update(bytes).digest("hex");
}

function git(args) {
  return execFileSync("git", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}

function errorText(error) {
  return error instanceof Error ? error.message.split("\n")[0] : String(error);
}

const blockers = [];

function check(condition, message) {
  if (!condition) {
    blockers.push(message);
  }
}

function loadPinnedJson(name, entry) {
  if (
    entry === null ||
    typeof entry !== "object" ||
    typeof entry.path !== "string" ||
    typeof entry.sha256 !== "string"
  ) {
    blockers.push(`${name} evidence declaration is invalid`);
    return { data: null, hash: null, path: null };
  }
  const path = join(root, entry.path);
  if (!existsSync(path)) {
    blockers.push(`${name} evidence is missing`);
    return { data: null, hash: null, path };
  }
  const hash = sha256(path);
  check(hash === entry.sha256, `${name} evidence hash changed`);
  try {
    return { data: json(path), hash, path };
  } catch (error) {
    blockers.push(`${name} evidence is not valid JSON: ${errorText(error)}`);
    return { data: null, hash, path };
  }
}

function validatePinnedDirectory(name, declaration) {
  const blockerStart = blockers.length;
  if (
    declaration === null ||
    typeof declaration !== "object" ||
    typeof declaration.path !== "string" ||
    typeof declaration.match !== "string" ||
    declaration.files === null ||
    typeof declaration.files !== "object"
  ) {
    blockers.push(`${name} declaration is invalid`);
    return { count: 0, status: "INVALID" };
  }
  const directory = join(root, declaration.path);
  if (!existsSync(directory)) {
    blockers.push(`${name} directory is missing`);
    return { count: 0, status: "INVALID" };
  }
  const pattern = new RegExp(declaration.match);
  const actualFiles = readdirSync(directory).filter((file) =>
    pattern.test(file),
  );
  const expectedFiles = Object.keys(declaration.files);
  actualFiles.sort();
  expectedFiles.sort();
  check(
    JSON.stringify(actualFiles) === JSON.stringify(expectedFiles),
    `${name} file set changed`,
  );
  for (const file of expectedFiles) {
    const path = join(directory, file);
    check(
      existsSync(path) && sha256(path) === declaration.files[file],
      `${name} file changed: ${file}`,
    );
  }
  return {
    count: actualFiles.length,
    status: blockers.length === blockerStart ? "VALID" : "INVALID",
  };
}

const predecessorProtocol = loadPinnedJson(
  "v1 protocol",
  protocol.reconciliation.predecessor.protocol,
);
const predecessorCheckerPath = join(
  root,
  protocol.reconciliation.predecessor.checker.path,
);
check(
  existsSync(predecessorCheckerPath) &&
    sha256(predecessorCheckerPath) ===
      protocol.reconciliation.predecessor.checker.sha256,
  "v1 checker changed",
);
const predecessorAggregate = loadPinnedJson(
  "v1 aggregate",
  protocol.reconciliation.predecessor.aggregate,
);
check(
  predecessorProtocol.data?.schema_version ===
    "distill.native-distribution-protocol/v1",
  "v1 protocol content changed",
);
check(
  predecessorAggregate.data?.status ===
    protocol.reconciliation.predecessor.status &&
    predecessorAggregate.data?.first_defect ===
      protocol.reconciliation.predecessor.first_defect,
  "v1 NO-GO verdict or first defect changed",
);

const coverage = loadPinnedJson("coverage", protocol.coverage.evidence);
const automated = loadPinnedJson(
  "automated Linux",
  protocol.linux.evidence.automated,
);
const suite = loadPinnedJson("Linux suite", protocol.linux.evidence.suite);
const fuzz = loadPinnedJson("standalone fuzz", protocol.linux.evidence.fuzz);
const packageEvidence = loadPinnedJson(
  "Linux package",
  protocol.linux.evidence.package,
);
const execution = loadPinnedJson(
  "Linux execution",
  protocol.linux.evidence.execution,
);
const macos = loadPinnedJson("macOS", protocol.macos.evidence);
const candidate = protocol.candidate_binding.candidate_revision;
const nativeTree = protocol.candidate_binding.native_tree;

let headRevision = null;
let headNativeTree = null;
let candidateNativeTree = null;
let nativeWorktreeStatus = null;
let finalWorktreeStatus = null;
try {
  headRevision = git(["rev-parse", "HEAD"]);
  headNativeTree = git(["rev-parse", "HEAD:native/distill-core"]);
  candidateNativeTree = git([
    "rev-parse",
    `${candidate}:native/distill-core`,
  ]);
  nativeWorktreeStatus = git([
    "status",
    "--porcelain=v1",
    "--untracked-files=all",
    "--",
    "native/distill-core",
  ]);
  if (!validateOnly) {
    finalWorktreeStatus = git([
      "status",
      "--porcelain=v1",
      "--untracked-files=all",
    ]);
  }
} catch (error) {
  blockers.push(`candidate or native tree cannot be resolved: ${errorText(error)}`);
}

check(
  protocol.reconciliation.native_tree_check_path ===
    "suite.native_prevalidation.native_tree",
  "v2 native tree ownership rule is invalid",
);
check(
  protocol.coverage.full_native_check_executions === 1 &&
    protocol.linux.executions === 1 &&
    protocol.linux.package_repetitions === 2 &&
    protocol.macos.executions === 1 &&
    protocol.aggregate.write_executions === 1 &&
    protocol.failure_policy.retry === false,
  "one-shot execution policy is invalid",
);
check(
  headNativeTree === nativeTree &&
    candidateNativeTree === nativeTree &&
    nativeWorktreeStatus === "",
  "candidate, HEAD, or native worktree tree differs from the preregistered tree",
);
if (!validateOnly) {
  check(finalWorktreeStatus === "", "final aggregation requires a clean worktree");
}

check(
  coverage.data?.status === "PASS" &&
    coverage.data?.native_tree === nativeTree &&
    coverage.data?.after?.lines?.percent >=
      protocol.coverage.required_line_percent &&
    coverage.data?.after?.branches?.percent >=
      protocol.coverage.required_branch_percent,
  "native coverage evidence is invalid",
);
check(
  execution.data?.status === "GO" &&
    execution.data?.candidate_revision === candidate &&
    execution.data?.native_tree === nativeTree &&
    execution.data?.linux_executions === 1 &&
    execution.data?.package_repetitions === 2 &&
    execution.data?.full_native_check_executions === 1 &&
    execution.data?.first_defect === null,
  "Linux one-shot execution receipt is not GO",
);
check(
  automated.data?.status === "GO" &&
    automated.data?.git_revision === candidate &&
    automated.data?.linux_release?.result === "passed" &&
    automated.data?.linux_release?.git_revision === candidate &&
    automated.data?.fuzz?.result === "clean" &&
    automated.data?.fuzz?.git_revision === candidate,
  "automated Linux evidence is not GO on the candidate",
);
check(
  suite.data?.result === "passed" &&
    suite.data?.git_revision === candidate &&
    suite.data?.native_prevalidation?.native_tree === nativeTree &&
    suite.data?.native_prevalidation?.evidence ===
      protocol.coverage.evidence.path,
  "standalone Linux suite does not bind coverage and native tree to the candidate",
);
check(
  fuzz.data?.result === "clean" &&
    fuzz.data?.git_revision === candidate &&
    fuzz.data?.cpu_seconds >= 3_600,
  "standalone Linux fuzz evidence is invalid",
);
check(
  packageEvidence.data?.status === "GO" &&
    packageEvidence.data?.candidate_revision === candidate &&
    packageEvidence.data?.native_tree === nativeTree &&
    packageEvidence.data?.repetitions === 2 &&
    packageEvidence.data?.byte_identical === true &&
    packageEvidence.data?.checksum_sidecars?.independently_validated === true &&
    packageEvidence.data?.checksum_sidecars?.first_sha256 ===
      packageEvidence.data?.checksum_sidecars?.second_sha256,
  "Linux package reproducibility or checksum evidence is invalid",
);
check(
  macos.data?.status === "GO" &&
    macos.data?.target === "macos-arm64" &&
    macos.data?.git_revision === candidate &&
    macos.data?.source_worktree_clean === true &&
    macos.data?.workflow_run_url === protocol.macos.workflow_run_url,
  "macOS receipt is not GO on the candidate revision",
);
const workflowPath = join(root, protocol.macos.workflow);
check(
  existsSync(workflowPath) &&
    sha256(workflowPath) === protocol.macos.workflow_sha256,
  "macOS workflow changed after qualification",
);

const historicalBlockerStart = blockers.length;
const historicalPrds = validatePinnedDirectory(
  "historical PRD",
  protocol.historical_integrity.prds,
);
const historicalLegacyEvidence = validatePinnedDirectory(
  "historical legacy evidence",
  protocol.historical_integrity.legacy_evidence,
);
const registry = loadPinnedJson(
  "historical qualification registry",
  protocol.historical_integrity.registry,
);
check(
  historicalPrds.count === protocol.historical_integrity.historical_prds &&
    historicalLegacyEvidence.count ===
      protocol.historical_integrity.historical_legacy_evidence &&
    Array.isArray(registry.data?.historical_evidence) &&
    registry.data.historical_evidence.length ===
      protocol.historical_integrity.historical_qualification_artifacts &&
    sha256(Buffer.from(JSON.stringify(registry.data.historical_evidence))) ===
      protocol.historical_integrity.registry.evidence_set_sha256,
  "historical integrity counts or evidence set changed",
);
if (Array.isArray(registry.data?.historical_evidence)) {
  for (const evidence of registry.data.historical_evidence) {
    check(
      typeof evidence?.path === "string" &&
        typeof evidence?.sha256 === "string" &&
        existsSync(join(root, evidence.path)) &&
        sha256(join(root, evidence.path)) === evidence.sha256,
      `historical qualification artifact changed: ${evidence?.path ?? "unknown"}`,
    );
  }
}
const historicalStatus =
  blockers.length === historicalBlockerStart ? "VALID" : "INVALID";

const nativeTreeBinding = {
  selected_path: protocol.reconciliation.native_tree_check_path,
  automated_native_tree:
    automated.data?.linux_release?.native_prevalidation?.native_tree ?? null,
  suite_native_tree:
    suite.data?.native_prevalidation?.native_tree ?? null,
};

if (validateOnly) {
  const result = {
    schema_version: "distill.native-distribution-validation/v2",
    qualification_id: protocol.qualification_id,
    mode: "validate-only",
    aggregate_written: false,
    candidate_revision: candidate,
    head_revision: headRevision,
    native_tree: nativeTree,
    native_tree_binding: nativeTreeBinding,
    first_defect: blockers[0] ?? null,
    status: blockers.length === 0 ? "VALID" : "INVALID",
  };
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
  if (blockers.length > 0) {
    process.exit(1);
  }
  process.exit(0);
}

const aggregatePath = join(root, protocol.aggregate.evidence);
if (existsSync(aggregatePath)) {
  throw new Error(`immutable aggregate path already exists: ${aggregatePath}`);
}

let remoteAttestation = null;
try {
  remoteAttestation =
    git([
      "ls-remote",
      "origin",
      protocol.candidate_binding.attestation_ref,
    ]).split(/\s+/)[0] || null;
} catch (error) {
  blockers.push(`remote attestation cannot be read: ${errorText(error)}`);
}
check(
  remoteAttestation === candidate,
  "published v2 attestation ref does not point to the candidate",
);

const aggregate = {
  schema_version: "distill.native-distribution-qualification/v2",
  qualification_id: protocol.qualification_id,
  candidate_revision: candidate,
  aggregation_revision: headRevision,
  native_tree: nativeTree,
  predecessor: {
    aggregate: protocol.reconciliation.predecessor.aggregate.path,
    status: predecessorAggregate.data?.status ?? null,
    first_defect: predecessorAggregate.data?.first_defect ?? null,
  },
  accepted_post_preregistration_event:
    protocol.reconciliation.accepted_post_preregistration_event,
  native_tree_binding: nativeTreeBinding,
  coverage: {
    evidence: protocol.coverage.evidence.path,
    sha256: coverage.hash,
    lines_percent: coverage.data?.after?.lines?.percent ?? null,
    branches_percent: coverage.data?.after?.branches?.percent ?? null,
    status: coverage.data?.status ?? null,
  },
  linux: {
    evidence: Object.fromEntries(
      Object.entries(protocol.linux.evidence).map(([name, entry]) => [
        name,
        entry.path,
      ]),
    ),
    hashes: {
      automated: automated.hash,
      suite: suite.hash,
      fuzz: fuzz.hash,
      package: packageEvidence.hash,
      execution: execution.hash,
    },
    status:
      automated.data?.status === "GO" &&
      suite.data?.result === "passed" &&
      fuzz.data?.result === "clean" &&
      packageEvidence.data?.status === "GO" &&
      execution.data?.status === "GO"
        ? "GO"
        : "NO-GO",
  },
  macos: {
    evidence: protocol.macos.evidence.path,
    sha256: macos.hash,
    workflow_run_url: macos.data?.workflow_run_url ?? null,
    status: macos.data?.status ?? null,
  },
  package_reproducibility: {
    archive_sha256: packageEvidence.data?.archive_sha256 ?? null,
    byte_identical: packageEvidence.data?.byte_identical ?? null,
    checksum_sidecars_valid:
      packageEvidence.data?.checksum_sidecars?.independently_validated ?? null,
  },
  historical_integrity: {
    historical_prds: historicalPrds.count,
    historical_legacy_evidence: historicalLegacyEvidence.count,
    historical_qualification_artifacts:
      registry.data?.historical_evidence?.length ?? null,
    registry_sha256: registry.hash,
    evidence_set_sha256:
      protocol.historical_integrity.registry.evidence_set_sha256,
    status: historicalStatus,
  },
  attestation: {
    ref: protocol.candidate_binding.attestation_ref,
    head: remoteAttestation,
  },
  first_defect: blockers[0] ?? null,
  status: blockers.length === 0 ? "GO" : "NO-GO",
};
writeFileSync(aggregatePath, `${JSON.stringify(aggregate, null, 2)}\n`, {
  flag: "wx",
});
process.stdout.write(`${JSON.stringify(aggregate, null, 2)}\n`);
if (blockers.length > 0) {
  process.exit(1);
}
