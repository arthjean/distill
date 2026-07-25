#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const baselineCommit = "704d50d6c1beb82abe442458a5e90eeac0611287";
const baselineTree = "2404877efa35c5917883389eb75a62af5c3ed571";
const inventoryHash =
  "10fb9d098c96c656dedce9750c84083677837ac4ff8f49bdccb89c39f05cc288";
const v5LedgerHash =
  "11e5b41b9787bb11b9c74825bacb1fd895bd7eea2c5ea6fdc6105718659e95b2";
const historicalEvidenceSetHash =
  "cac91ac888363739288cac392198669c7744efded6686cfedbff5d56e5ffbfbf";
const approvedLegacyReadmeHash =
  "6ef32d769db804171af388106d9a05c8dfe931018553691c833270acc381ab63";

const historicalPrds = {
  "prd-distill-audit-fixes.md":
    "11903c895d3b6964ff314ca315bf3072f5552c590a170795c60ce4596e32a8ad",
  "prd-distill-context-projection-engine.md":
    "adef99cc9f18c4cea71dd2580f4b65797a5de85d9e14a3ad6862d06426505dd6",
  "prd-distill-v010-claude-code-alignment.md":
    "2d5d5870ea0034ff106d9d473e164e753a6ec96886cd3324d5a6a19ef42889f4",
  "prd-distill-v011-audit-remediation.md":
    "4f369841e13320632e93326a945eca7b020c1837615c7a821081a4b5ebb24e01",
  "prd-distill-v090-security.md":
    "d9a1e96719c8dd9714e360c8e4bdaaa321ac5a6fb2fa158abfea4af0d9e9ac5a",
  "prd-distill-v091-audit-cleanup.md":
    "b259ab6e1004c6ca3cdcf25bd1124ecd0e1a12d050d4338474bc7d31acc55d65",
  "prd-distill-v092-hardening-cleanup.md":
    "315a04a07c8b07ce1ed86281568767b8040877bc80ed05100d9977342203e167",
  "prd-distill-v1-phase2.md":
    "4bb000bddc2f1ecc63e3d78348b9f66cc2202f6614a682d0dfc6dbd35dd9231a",
  "prd-distill-v1.md":
    "1a6734768a1620742460f0b7175814e90f1fc54c8df70645eb508a44c9dd5cb1",
};

const historicalLegacyEvidence = {
  "baseline.md":
    "ee9524a2405f0154026887a968dd2b642926a4fdcbaa46e03b3140f7c42c8340",
  "evidence.json":
    "6499bae5585dddeadb88c4fefe47456b13d9273bfebc6f7de33dedeea53cad1e",
  "run.mjs": "e7c042f67df6ad80ee77e70223a6fd102b24332ab7971c015a94f099b95f9788",
};

function fail(message) {
  process.stderr.write(`US-019 validation failed: ${message}\n`);
  process.exit(1);
}

function read(path) {
  return readFileSync(join(root, path));
}

function json(path) {
  return JSON.parse(read(path).toString("utf8"));
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function git(args) {
  return execFileSync("git", args, {
    cwd: root,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function assertGitUnchanged(path) {
  try {
    execFileSync("git", ["diff", "--quiet", baselineCommit, "--", path], {
      cwd: root,
      stdio: "ignore",
    });
  } catch {
    fail(`${path} changed relative to ${baselineCommit}`);
  }
}

const recordedTree = git([
  "rev-parse",
  `${baselineCommit}:packages/mcp-server`,
]).trim();
if (recordedTree !== baselineTree) {
  fail(`legacy tree is ${recordedTree}, expected ${baselineTree}`);
}

const legacyChanges = git([
  "diff",
  "--name-only",
  baselineCommit,
  "--",
  "packages/mcp-server",
])
  .trim()
  .split("\n")
  .filter(Boolean);
if (
  JSON.stringify(legacyChanges) !==
    JSON.stringify(["packages/mcp-server/README.md"]) ||
  sha256(read("packages/mcp-server/README.md")) !== approvedLegacyReadmeHash
) {
  fail("legacy package changed outside the approved README migration update");
}
assertGitUnchanged(".github/workflows");
if (
  git([
    "status",
    "--porcelain=v1",
    "--untracked-files=all",
    "--",
    "native/distill-core",
  ]).trim() !== ""
) {
  fail("native source worktree contains tracked or untracked changes");
}

const expectedInventory = `${git([
  "ls-tree",
  "-r",
  "--name-only",
  baselineCommit,
  "--",
  "packages/mcp-server",
]).trim()}\n`;
const inventory = read("docs/migration/legacy-deletion-files.txt");
if (inventory.toString("utf8") !== expectedInventory) {
  fail("file-level deletion inventory does not match the attested legacy tree");
}
if (sha256(inventory) !== inventoryHash) {
  fail("file-level deletion inventory hash changed");
}
if (expectedInventory.trim().split("\n").length !== 208) {
  fail("attested legacy inventory is not 208 files");
}

const tasksDirectory = join(root, "tasks");
const actualPrds = readdirSync(tasksDirectory)
  .filter((name) => /^prd-distill-.*\.md$/.test(name))
  .sort();
const expectedPrds = Object.keys(historicalPrds).sort();
if (JSON.stringify(actualPrds) !== JSON.stringify(expectedPrds)) {
  fail("historical PRD file set changed");
}
for (const [name, expectedHash] of Object.entries(historicalPrds)) {
  const actualHash = sha256(readFileSync(join(tasksDirectory, name)));
  if (actualHash !== expectedHash) {
    fail(`${name} is not byte-identical`);
  }
}

const legacyEvidenceDirectory = join(root, "evaluation/legacy");
const actualLegacyEvidence = readdirSync(legacyEvidenceDirectory).sort();
const expectedLegacyEvidence = Object.keys(historicalLegacyEvidence).sort();
if (
  JSON.stringify(actualLegacyEvidence) !==
  JSON.stringify(expectedLegacyEvidence)
) {
  fail("historical legacy evidence file set changed");
}
for (const [name, expectedHash] of Object.entries(historicalLegacyEvidence)) {
  const actualHash = sha256(readFileSync(join(legacyEvidenceDirectory, name)));
  if (actualHash !== expectedHash) {
    fail(`evaluation/legacy/${name} changed`);
  }
}

const v5LedgerBytes = read(
  "evaluation/release/paired-qualification-v5-ledger.json",
);
if (sha256(v5LedgerBytes) !== v5LedgerHash) {
  fail("v5 authorization ledger changed relative to the attested baseline");
}
const v5Ledger = JSON.parse(v5LedgerBytes.toString("utf8"));
if (
  v5Ledger.historical_evidence.length !== 34 ||
  sha256(JSON.stringify(v5Ledger.historical_evidence)) !==
    historicalEvidenceSetHash
) {
  fail("required historical qualification path/hash set changed");
}
for (const evidence of v5Ledger.historical_evidence) {
  if (sha256(read(evidence.path)) !== evidence.sha256) {
    fail(`historical qualification artifact changed: ${evidence.path}`);
  }
}

if (process.argv.includes("--historical-only")) {
  process.stdout.write(
    JSON.stringify(
      {
        status: "VALID",
        baseline_commit: baselineCommit,
        legacy_tree: baselineTree,
        legacy_files: 208,
        historical_prds: expectedPrds.length,
        historical_legacy_evidence: expectedLegacyEvidence.length,
        historical_qualification_artifacts:
          v5Ledger.historical_evidence.length,
      },
      null,
      2,
    ) + "\n",
  );
  process.exit(0);
}

const qualification = json(
  "evaluation/release/evidence/us018-qualification-v5.json",
);
const requiredQualificationControls = [
  "same_clean_source_revision",
  "authorization_consumption_valid",
  "paired_evidence_valid",
  "reported_tasks_valid",
  "reported_model_controls_valid",
  "aggregate_evidence_valid",
  "execution_ledger_valid",
  "invocation_outcomes_valid",
  "early_stop_mechanically_valid",
  "paired_gate_go",
  "attestation_chain_valid",
  "remote_attestation_valid",
  "macos_evidence_valid",
];
if (
  qualification.status !== "GO" ||
  requiredQualificationControls.some((key) => qualification[key] !== true)
) {
  fail("US-018 v5 aggregate is not an internally valid GO");
}

const distribution = json("docs/distribution/native-assets.json");
const currentNativeTree = git(["rev-parse", "HEAD:native/distill-core"]).trim();
const linuxAsset = distribution.assets?.find(
  (asset) => asset.platform === "linux-x86_64",
);
const macosAsset = distribution.assets?.find(
  (asset) => asset.platform === "macos-arm64",
);
const nativeQualification = json(
  "evaluation/release/evidence/native-distribution-v2.json",
);
if (
  distribution.strategy !== "direct-native-release-assets" ||
  distribution.current_native_tree !== currentNativeTree ||
  distribution.qualification?.aggregate !==
    "evaluation/release/evidence/native-distribution-v2.json" ||
  distribution.qualification?.candidate_revision !==
    nativeQualification.candidate_revision ||
  distribution.qualification?.native_tree !== currentNativeTree ||
  distribution.qualification?.status !== "GO" ||
  !distribution.checksum_scope?.startsWith("integrity only") ||
  distribution.release_performed !== false ||
  distribution.version_changed !== false ||
  distribution.npm_launcher?.selected !== false ||
  JSON.stringify(distribution.assets.map((asset) => asset.platform)) !==
    JSON.stringify(["linux-x86_64", "macos-arm64"]) ||
  linuxAsset?.status !== "qualified" ||
  linuxAsset.qualification?.evidence !==
    "evaluation/release/evidence/automated-linux-x86_64-v2.json" ||
  linuxAsset.qualification?.suite_evidence !==
    "evaluation/release/evidence/suite-linux-x86_64-v2.json" ||
  linuxAsset.qualification?.package_evidence !==
    "evaluation/release/evidence/package-linux-x86_64-v1.json" ||
  linuxAsset.qualification?.aggregate !==
    "evaluation/release/evidence/native-distribution-v2.json" ||
  linuxAsset.qualification?.native_tree !== currentNativeTree ||
  linuxAsset.qualification?.same_as_current_native_tree !== true ||
  macosAsset?.status !== "qualified" ||
  macosAsset.qualification?.evidence !==
    "evaluation/release/evidence/macos-arm64-distribution-v1.json" ||
  macosAsset.qualification?.aggregate !==
    "evaluation/release/evidence/native-distribution-v2.json" ||
  macosAsset.qualification?.native_tree !== currentNativeTree ||
  macosAsset.qualification?.same_as_current_native_tree !== true ||
  nativeQualification.status !== "GO" ||
  nativeQualification.first_defect !== null ||
  nativeQualification.native_tree !== currentNativeTree ||
  nativeQualification.native_tree_binding?.selected_path !==
    "suite.native_prevalidation.native_tree" ||
  nativeQualification.coverage?.evidence !==
    "evaluation/release/evidence/native-coverage-v1.json" ||
  nativeQualification.attestation?.ref !==
    "refs/distill/qualifications/native-distribution-v2-20260725" ||
  nativeQualification.attestation?.head !==
    nativeQualification.candidate_revision
) {
  fail("native distribution decision is incomplete or expanded");
}
if (
  git([
    "rev-parse",
    `${linuxAsset.qualification.git_revision}:native/distill-core`,
  ]).trim() !== linuxAsset.qualification.native_tree ||
  git([
    "rev-parse",
    `${macosAsset.qualification.git_revision}:native/distill-core`,
  ]).trim() !== macosAsset.qualification.native_tree ||
  json(linuxAsset.qualification.evidence).status !== "GO" ||
  json(macosAsset.qualification.evidence).status !== "GO" ||
  nativeQualification.candidate_revision !==
    linuxAsset.qualification.git_revision ||
  nativeQualification.candidate_revision !==
    macosAsset.qualification.git_revision
) {
  fail("native asset qualification provenance is invalid");
}

const remoteAttestation = git([
  "ls-remote",
  "origin",
  nativeQualification.attestation.ref,
])
  .trim()
  .split(/\s+/)[0];
if (remoteAttestation !== nativeQualification.candidate_revision) {
  fail("remote native distribution v2 attestation changed");
}

const rootPackage = json("package.json");
if (
  rootPackage.scripts?.["build:native"] !==
    "cargo build --locked --release --manifest-path native/distill-core/Cargo.toml" ||
  rootPackage.scripts?.["check:migration"] !== "bun scripts/check-us019.mjs" ||
  rootPackage.scripts?.["package:native"] !== "./scripts/package-native.sh"
) {
  fail("root native build and packaging commands are not prepared");
}

const legacyPackage = json("packages/mcp-server/package.json");
if (
  legacyPackage.version !== "0.11.2" ||
  legacyPackage.dependencies?.["web-tree-sitter"] !== "0.22.6" ||
  legacyPackage.dependencies?.["@sebastianwessel/quickjs"] !== "3.0.0"
) {
  fail("legacy version or pinned dependencies changed");
}

if ((statSync(join(root, "scripts/package-native.sh")).mode & 0o111) === 0) {
  fail("native packaging script is not executable");
}

const migration = read("docs/migration/mcp-first-to-native.md").toString(
  "utf8",
);
for (const requiredText of [
  "`auto_optimize`",
  "`smart_file_read`",
  "`code_execute`",
  "Resolved US-004 salvage matrix",
  "Unsupported platform matrix",
  "same-tree Linux and macOS qualification is `GO`",
]) {
  if (!migration.includes(requiredText)) {
    fail(`migration documentation is missing ${requiredText}`);
  }
}

const deletionPlan = read("docs/migration/legacy-deletion-plan.md").toString(
  "utf8",
);
for (const requiredText of [
  "native-distribution-v2",
  "`evaluation/legacy/run.mjs`",
  "explicitly ignore that archival",
  "`distill-mcp`, `expect-type`, `turbo`, and `typescript`",
  "native-distribution-v2 `GO`",
]) {
  if (!deletionPlan.includes(requiredText)) {
    fail(`deletion plan is missing ${requiredText}`);
  }
}

const status = json("tasks/prd-distill-context-projection-engine-status.json");
const us020 = status.stories?.find((story) => story.id === "US-020");
if (
  status.prd?.status !== "IN_PROGRESS" ||
  status.epics?.find((epic) => epic.id === "EP-005")?.status !==
    "IN_PROGRESS" ||
  us020?.status !== "TODO" ||
  us020?.started_at !== null
) {
  fail("US-020 is not blocked on separate human authorization");
}

process.stdout.write(
  JSON.stringify(
    {
      status: "VALID",
      baseline_commit: baselineCommit,
      legacy_tree: baselineTree,
      legacy_files: 208,
      historical_prds: expectedPrds.length,
      historical_legacy_evidence: expectedLegacyEvidence.length,
      historical_qualification_artifacts: v5Ledger.historical_evidence.length,
      qualification: qualification.status,
      distribution: distribution.strategy,
      linux_asset: linuxAsset.status,
      macos_asset: macosAsset.status,
    },
    null,
    2,
  ) + "\n",
);
