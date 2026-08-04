/**
 * Aggregate verdict for the executed-path qualification, EP-006 US-019.
 *
 * The paired report alone is not a release verdict. This checker reruns every
 * preregistered contract, integrity, and zero-network gate on the candidate,
 * verifies that the report binds the same protocol, source tree, and binary,
 * and only then writes the aggregate receipt beside the paired evidence,
 * outside the closed `evaluation/release/evidence/` tree.
 *
 *   bun evaluation/release/check-executed-path-v1.mjs
 *
 * It writes nothing when a gate fails or the paired report is absent.
 */
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const protocolPath = join(root, "evaluation/release/executed-path-v1-protocol.json");
const ledgerPath = join(root, "evaluation/release/executed-path-v1-ledger.json");

const protocolBytes = await readFile(protocolPath);
const protocol = JSON.parse(protocolBytes);
const ledgerBytes = await readFile(ledgerPath);
const ledger = JSON.parse(ledgerBytes);
const protocolSha256 = sha256(protocolBytes);

if (ledger.protocol_sha256 !== protocolSha256) {
  throw new Error("the authorization ledger does not bind this protocol");
}
for (const entry of ledger.historical_evidence) {
  if (sha256(await readFile(join(root, entry.path))) !== entry.sha256) {
    throw new Error(`closed evidence changed: ${entry.path}`);
  }
}

const stateDirectory = protocol.execution_policy.evidence_outside_worktree;
const reportPath = join(stateDirectory, ledger.execution.report_name);
const aggregatePath = join(stateDirectory, ledger.execution.aggregate_report_name);
if (aggregatePath.startsWith(join(root, "evaluation/release/evidence"))) {
  throw new Error("the aggregate receipt may never be written into the closed evidence tree");
}

// Gates run first: they qualify the candidate whether or not a paired run has
// happened, and a failing gate stops the aggregate before it can be written.
const gates = protocol.gates.required_gate_commands.map(runGate);
const gatesPassed = gates.every((gate) => gate.passed);
for (const gate of gates) {
  process.stderr.write(`${gate.passed ? "PASS" : "FAIL"} ${gate.command}\n`);
}

let reportBytes;
try {
  reportBytes = await readFile(reportPath);
} catch {
  process.stderr.write(
    `${reportPath} is absent: run bun evaluation/release/run-executed-path-v1.mjs --execute under a consumed authorization first\n`,
  );
  process.exit(1);
}
const report = JSON.parse(reportBytes);
const executionLedgerBytes = await readFile(
  join(stateDirectory, ledger.execution.execution_ledger_name),
);
const executionLedger = JSON.parse(executionLedgerBytes);

const bindings = {
  protocol_sha256: report.evaluated_inputs.protocol_sha256 === protocolSha256,
  ledger_sha256: report.evaluated_inputs.ledger_sha256 === sha256(ledgerBytes),
  source_tree: report.evaluated_inputs.source_tree === protocol.candidate.source_tree,
  binary_sha256: report.evaluated_inputs.native_binary_sha256 === protocol.candidate.binary_sha256,
  binary_unchanged:
    report.evaluated_inputs.native_binary_sha256_after_execution ===
    protocol.candidate.binary_sha256,
  control_binary_sha256:
    report.conditions.baseline.control_binary_sha256 === protocol.control.binary_sha256,
  control_revision: report.conditions.baseline.control_revision === protocol.control.source_revision,
  unbounded_raw_not_measured: report.conditions.unbounded_raw.measured === false,
  worktree_clean: report.evaluated_inputs.source_worktree_clean === true,
  execution_ledger_sha256:
    report.evaluated_inputs.execution_ledger_sha256 === sha256(executionLedgerBytes),
  real_corpus_manifest_sha256:
    report.evaluated_inputs.real_corpus_manifest_sha256 === protocol.corpus.real_manifest_sha256,
  baseline_sha256: report.evaluated_inputs.baseline_sha256 === protocol.baseline.sha256,
  invocation_ceiling_respected:
    executionLedger.invocations.length <= protocol.sample.maximum_invocations,
  source_tree_still_current: currentSourceTree() === protocol.candidate.source_tree,
};
const bound = Object.values(bindings).every(Boolean);
const status = bound && gatesPassed && report.status === "GO" ? "GO" : "NO-GO";

const aggregate = {
  schema_version: "distill.executed-path-aggregate/v1",
  qualification_id: protocol.qualification_id,
  produced_at: new Date().toISOString(),
  status,
  paired: {
    status: report.status,
    path: reportPath,
    sha256: sha256(reportBytes),
    sample_size: report.scoring.sample_size,
    baseline_successes: report.scoring.baseline_successes,
    projected_successes: report.scoring.projected_successes,
    delta_percentage_points: report.scoring.delta_percentage_points,
    minimum_delta_percentage_points: protocol.gates.minimum_delta_percentage_points,
    termination: report.termination,
  },
  conditions: report.conditions,
  bindings,
  gates,
  claim_scope: {
    executed_profile: protocol.executed_path.default_preservation_profile,
    control_profile: protocol.control.default_preservation_profile,
    control_revision: protocol.control.source_revision,
    per_category_profile_selection: false,
    unbounded_raw_condition_measured: false,
    note: "Every number in this receipt was produced under a preservation profile some product surface executes: auto/v1 for the candidate, plain-text/v1 for the release it replaces. No claim may cite a non-executed profile, and none may assert parity with unbounded context, which this qualification did not measure.",
  },
};

if (status !== "GO") {
  process.stderr.write(
    `${JSON.stringify({ status, bindings, gates: gates.map((gate) => ({ command: gate.command, passed: gate.passed })), paired: report.status }, null, 2)}\n`,
  );
  process.stderr.write("aggregate is NO-GO; no receipt written\n");
  process.exit(1);
}
await writeJsonAtomic(aggregatePath, aggregate);
process.stderr.write(`${aggregatePath}: ${status}\n`);

function runGate(command) {
  const argv = command.split(" ").filter(Boolean);
  const started = Date.now();
  const result = spawnSync(argv[0], argv.slice(1), {
    cwd: root,
    encoding: "utf8",
    env: process.env,
    maxBuffer: 32 * 1024 * 1024,
  });
  return {
    command,
    passed: result.status === 0,
    exit_code: result.status,
    duration_ms: Date.now() - started,
    stderr_tail: bounded(result.stderr),
  };
}

function currentSourceTree() {
  const result = spawnSync("bash", [join(root, "scripts/source-tree.sh")], { encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : null;
}

async function writeJsonAtomic(path, value) {
  const temporary = `${path}.tmp`;
  await writeFile(temporary, `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 });
  await rename(temporary, path);
}

function bounded(value) {
  return String(value ?? "").trim().slice(-1000);
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
