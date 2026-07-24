import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const evidenceRoot = join(root, "evaluation/release/evidence");
const ledger = await readJson(join(root, "evaluation/release/paired-attempt-ledger.json"));
const paired = await readJson(join(evidenceRoot, "paired-tasks.json"));
const macos = await readJson(join(evidenceRoot, "macos-arm64.json"));
const output = join(evidenceRoot, "us018-qualification.json");

const ledgerErrors = [];
let totalInvocations = 0;
for (const entry of ledger.attempts) {
  const bytes = await readFile(join(root, entry.path));
  const digest = sha256(bytes);
  if (digest !== entry.sha256) {
    ledgerErrors.push(`attempt ${entry.attempt} digest mismatch`);
  }
  const report = JSON.parse(bytes);
  if (report.phase !== "complete" || report.status !== entry.status) {
    ledgerErrors.push(`attempt ${entry.attempt} status mismatch`);
  }
  if (report.scoring?.sample_size !== 20 || entry.invocations !== 40) {
    ledgerErrors.push(`attempt ${entry.attempt} invocation count is not 40`);
  }
  totalInvocations += entry.invocations;
}
if (
  totalInvocations !== ledger.total_invocations ||
  totalInvocations > ledger.maximum_qualification_invocations
) {
  ledgerErrors.push("paired qualification invocation ceiling is invalid");
}

const sameRevision =
  paired.evaluated_inputs?.git_revision === macos.git_revision &&
  paired.evaluated_inputs?.source_worktree_clean === true &&
  macos.source_worktree_clean === true;
const macosEvidenceValid =
  macos.schema_version === "distill.macos-qualification/v1" &&
  macos.target === "macos-arm64" &&
  macos.machine?.architecture === "arm64" &&
  macos.source_worktree_clean === true &&
  typeof macos.binary_sha256 === "string" &&
  /^[0-9a-f]{64}$/.test(macos.binary_sha256) &&
  macos.release_suite?.format === "passed" &&
  macos.release_suite?.clippy === "passed" &&
  macos.release_suite?.contract_and_corpus_tests === "passed" &&
  macos.release_suite?.build === "passed" &&
  macos.surfaces?.codex?.install === "installed" &&
  macos.surfaces?.codex?.repeat === "unchanged" &&
  macos.surfaces?.codex?.uninstall === "restored_absent" &&
  macos.surfaces?.claude?.install === "installed" &&
  macos.surfaces?.claude?.repeat === "unchanged" &&
  macos.surfaces?.claude?.uninstall === "restored_absent" &&
  macos.surfaces?.project?.fidelity === "extractive" &&
  macos.surfaces?.project?.byte_exact_restore === true;
const blockers = [
  ...(paired.status === "GO" ? [] : [`paired task gate is ${paired.status}`]),
  ...(macos.status === "GO" && macosEvidenceValid
    ? []
    : ["macOS arm64 evidence is incomplete or not GO"]),
  ...(sameRevision ? [] : ["paired and macOS evidence do not identify the same clean revision"]),
  ...ledgerErrors,
];
const report = {
  schema_version: "distill.us018-qualification/v1",
  generated_at: new Date().toISOString(),
  paired: {
    status: paired.status,
    git_revision: paired.evaluated_inputs?.git_revision ?? null,
    report_sha256: sha256(await readFile(join(evidenceRoot, "paired-tasks.json"))),
    total_invocations: totalInvocations,
  },
  macos: {
    status: macos.status,
    git_revision: macos.git_revision,
    binary_sha256: macos.binary_sha256 ?? null,
    workflow_run_url: macos.workflow_run_url ?? null,
  },
  same_clean_source_revision: sameRevision,
  macos_evidence_valid: macosEvidenceValid,
  blockers,
  status: blockers.length === 0 ? "GO" : "NO-GO",
};
await mkdir(dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
if (report.status !== "GO") {
  process.exitCode = 1;
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
