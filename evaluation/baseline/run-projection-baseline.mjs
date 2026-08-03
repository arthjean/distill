import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { percentile } from "../corpus/lib.mjs";
import { parseRealManifest, sha256, validateRealCorpus } from "../corpus/real/lib.mjs";

const BASELINE_SCHEMA_VERSION = "distill.projection-baseline/v1";
const EXECUTED_PROFILE = "plain-text/v1";

/**
 * The executed default first, then three additional budgets: the corpus token
 * profile, a large budget, and the zero-payload boundary that proves typed
 * failures are recorded rather than aborting the run.
 */
const BUDGETS = [
  {
    id: "codex-hook-default",
    unit: "tokens",
    total_visible_limit: 2250,
    reserved_envelope: 450,
    role: "executed Codex hook default (SAFE_OUTPUT_CAP_TOKENS, DEFAULT_RESERVED_TOKENS)",
  },
  {
    id: "tokens-512-envelope-64",
    unit: "tokens",
    total_visible_limit: 512,
    reserved_envelope: 64,
    role: "corpus token budget profile",
  },
  {
    id: "tokens-8192-envelope-1024",
    unit: "tokens",
    total_visible_limit: 8192,
    reserved_envelope: 1024,
    role: "large host budget",
  },
  {
    id: "tokens-450-envelope-450",
    unit: "tokens",
    total_visible_limit: 450,
    reserved_envelope: 450,
    role: "zero payload boundary",
  },
];

/**
 * Measurements the PRD problem statement records for the current tree. Utilization
 * must reproduce within one percentage point. The original token count must stay
 * within its declared fraction of the recorded count: the corpus stores scrubbed
 * bytes, so a fixture whose capture normalized identities counts slightly fewer
 * tokens than the raw command did, while a wrong fixture still fails closed.
 */
const REFERENCES = [
  {
    fixture: "source-artifact-rs",
    budget: "codex-hook-default",
    declared_original_count: 2799,
    declared_utilization_percent: 0.39,
    tolerance_points: 1,
    count_tolerance_fraction: 0.1,
    note: "line-structured source file, captured without any normalization hit",
  },
  {
    fixture: "log-git-log-stat",
    budget: "codex-hook-default",
    declared_original_count: 6341,
    declared_utilization_percent: 2.4,
    tolerance_points: 1,
    count_tolerance_fraction: 0.1,
    note: "command log; scrubbing author identities shortens the captured bytes",
  },
];

const directory = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(directory, "../..");
const realDirectory = join(repoRoot, "evaluation/corpus/real");
const realManifestPath = join(realDirectory, "manifest.jsonl");
const binaryPath = join(repoRoot, "target/release/distill");
const outputPath = join(directory, "projection-baseline-v1.json");

if (existsSync(outputPath) && !process.argv.includes("--force")) {
  throw new Error(
    "evaluation/baseline/projection-baseline-v1.json is frozen evidence; rewriting requires --force",
  );
}
if (!existsSync(binaryPath)) {
  throw new Error("target/release/distill is missing; run cargo build --locked --release first");
}

const manifestBytes = readFileSync(realManifestPath);
const records = parseRealManifest(manifestBytes.toString("utf8"));
const corpusSummary = validateRealCorpus(records, (path) => readFileSync(join(realDirectory, path)));

const store = mkdtempSync(join(tmpdir(), "distill-baseline-store-"));
let fixtures;
try {
  fixtures = records.map((record) => measureFixture(record, store));
} finally {
  rmSync(store, { recursive: true, force: true });
}

const references = REFERENCES.map((reference) => resolveReference(reference, fixtures));
const unreproduced = references.filter((reference) => !reference.reproduced);
if (unreproduced.length > 0) {
  throw new Error(
    `baseline did not reproduce the recorded measurements: ${JSON.stringify(unreproduced)}`,
  );
}

const evidence = {
  schema_version: BASELINE_SCHEMA_VERSION,
  id: "projection-baseline-v1",
  recorded_at: new Date().toISOString().slice(0, 10),
  status: "FROZEN",
  purpose:
    "Executed-path projection behavior before EP-002, recorded per real corpus fixture and budget.",
  executed_profile: EXECUTED_PROFILE,
  executed_profile_source: "src/surface.rs DEFAULT_PRESERVATION_PROFILE, pinned by both surfaces",
  measured_through: "target/release/distill project --json, the CLI seam over Engine::handle",
  source_revision: gitOutput(["rev-parse", "HEAD"]),
  worktree_clean: gitOutput(["status", "--porcelain"]).length === 0,
  binary_sha256: sha256(readFileSync(binaryPath)),
  corpus_manifest_sha256: sha256(manifestBytes),
  corpus: corpusSummary,
  budgets: BUDGETS,
  references,
  aggregates: BUDGETS.map((budget) => aggregate(budget, fixtures)),
  fixtures,
};

writeFileSync(outputPath, `${JSON.stringify(evidence, null, 2)}\n`, {
  encoding: "utf8",
  mode: 0o644,
});
console.log(
  JSON.stringify({
    mode: "write",
    output: "evaluation/baseline/projection-baseline-v1.json",
    fixtures: fixtures.length,
    budgets: BUDGETS.length,
    references: references.length,
    aggregates: evidence.aggregates.map((entry) => ({
      budget: entry.budget,
      median_budget_utilization_percent: entry.median_budget_utilization_percent,
    })),
  }),
);

function measureFixture(record, store) {
  const bytes = readFileSync(join(realDirectory, record.path));
  return {
    id: record.id,
    shape: record.shape,
    byte_length: record.byte_length,
    line_count: record.line_count,
    results: BUDGETS.map((budget) => project(bytes, budget, store)),
  };
}

function project(bytes, budget, store) {
  const result = Bun.spawnSync({
    cmd: [
      binaryPath,
      "--store",
      join(store, "artifacts.db"),
      "project",
      "--budget",
      String(budget.total_visible_limit),
      "--reserve",
      String(budget.reserved_envelope),
      "--unit",
      budget.unit,
      "--profile",
      EXECUTED_PROFILE,
      "--json",
    ],
    stdin: bytes,
    stdout: "pipe",
    stderr: "pipe",
    env: { PATH: "/usr/bin:/bin", NO_COLOR: "1" },
  });

  const stdout = Buffer.from(result.stdout).toString("utf8").trim();
  if (stdout.length === 0) {
    throw new Error(
      `projection produced no protocol output for budget ${budget.id}: ${Buffer.from(
        result.stderr,
      ).toString("utf8")}`,
    );
  }
  const envelope = JSON.parse(stdout);
  const payloadLimit = budget.total_visible_limit - budget.reserved_envelope;
  if (envelope.ok !== true) {
    // A typed failure is recorded rather than aborting the baseline run.
    return {
      budget: budget.id,
      payload_limit: payloadLimit,
      exit_code: result.exitCode,
      failure_code: envelope.error.code,
      failure_message: envelope.error.message,
    };
  }

  const receipt = envelope.result.receipt;
  const retainedBytes = spanBytes(receipt.retained_spans);
  const sourceBytes = envelope.result.artifact.source_bytes;
  return {
    budget: budget.id,
    payload_limit: payloadLimit,
    exit_code: result.exitCode,
    fidelity: receipt.fidelity,
    original_count: receipt.original_count,
    visible_count: receipt.visible_count,
    over_budget: receipt.original_count > payloadLimit,
    budget_utilization_percent: ratio(receipt.visible_count, payloadLimit),
    retained_byte_ratio_percent: ratio(retainedBytes, sourceBytes),
    retained_bytes: retainedBytes,
    retained_span_count: receipt.retained_spans.length,
    omitted_span_count: receipt.omitted_spans.length,
  };
}

function aggregate(budget, fixtures) {
  const results = fixtures.map((fixture) =>
    fixture.results.find((result) => result.budget === budget.id),
  );
  const failures = new Map();
  for (const result of results.filter((entry) => entry.failure_code !== undefined)) {
    failures.set(result.failure_code, (failures.get(result.failure_code) ?? 0) + 1);
  }
  const overBudget = results.filter((result) => result.over_budget === true);
  const utilization = overBudget.map((result) => result.budget_utilization_percent).sort(ascending);
  const retention = overBudget.map((result) => result.retained_byte_ratio_percent).sort(ascending);

  return {
    budget: budget.id,
    payload_limit: budget.total_visible_limit - budget.reserved_envelope,
    fixtures: results.length,
    exact_fidelity: results.filter((result) => result.fidelity === "exact").length,
    extractive_fidelity: results.filter((result) => result.fidelity === "extractive").length,
    typed_failures: Object.fromEntries([...failures].sort(([left], [right]) =>
      left.localeCompare(right),
    )),
    over_budget_fixtures: overBudget.length,
    min_budget_utilization_percent: utilization[0] ?? null,
    median_budget_utilization_percent: percentile(utilization, 0.5),
    max_budget_utilization_percent: utilization.at(-1) ?? null,
    median_retained_byte_ratio_percent: percentile(retention, 0.5),
  };
}

function resolveReference(reference, fixtures) {
  const fixture = fixtures.find((candidate) => candidate.id === reference.fixture);
  const result = fixture?.results.find((candidate) => candidate.budget === reference.budget);
  const utilization = result?.budget_utilization_percent ?? null;
  const countDelta =
    result === undefined ? null : result.original_count - reference.declared_original_count;
  const reproduced =
    result !== undefined &&
    Math.abs(countDelta) <=
      reference.declared_original_count * reference.count_tolerance_fraction &&
    Math.abs(utilization - reference.declared_utilization_percent) <= reference.tolerance_points;
  return {
    ...reference,
    measured_original_count: result?.original_count ?? null,
    measured_visible_count: result?.visible_count ?? null,
    measured_utilization_percent: utilization,
    original_count_delta: countDelta,
    reproduced,
  };
}

function spanBytes(spans) {
  return spans.reduce((total, span) => total + (span.end - span.start), 0);
}

function ratio(part, whole) {
  return whole === 0 ? 0 : Math.round((part / whole) * 1_000_000) / 10_000;
}

function ascending(left, right) {
  return left - right;
}

function gitOutput(args) {
  const git = Bun.which("git");
  if (git === null) {
    return "";
  }
  const result = Bun.spawnSync({ cmd: [git, ...args], cwd: repoRoot });
  return result.exitCode === 0 ? Buffer.from(result.stdout).toString("utf8").trim() : "";
}
