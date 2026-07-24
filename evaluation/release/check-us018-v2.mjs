import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const releaseRoot = join(root, "evaluation/release");
const evidenceRoot = join(releaseRoot, "evidence");
const manifestBytes = await readFile(join(releaseRoot, "paired-tasks-v2.json"));
const manifest = JSON.parse(manifestBytes);
const authorizationPath = join(
  releaseRoot,
  "paired-qualification-v2-ledger.json",
);
const authorizationBytes = await readFile(authorizationPath);
const authorization = JSON.parse(authorizationBytes);
const pairedPath = join(evidenceRoot, "paired-tasks-v2.json");
const executionLedgerPath = join(
  evidenceRoot,
  "paired-tasks-v2-execution-ledger.json",
);
const macosPath = join(evidenceRoot, "macos-arm64-v2.json");
const paired = await readJson(pairedPath);
const executionLedger = await readJson(executionLedgerPath);
const macos = await readJson(macosPath);
const output = join(evidenceRoot, "us018-qualification-v2.json");
const canonicalBinary = join(
  root,
  "native/distill-core/target/release/distill",
);
const canonicalBinarySha256 = sha256(await readFile(canonicalBinary));

const blockers = [];
if (
  authorization.schema_version !== "distill.paired-qualification-ledger/v2" ||
  authorization.qualification_id !== manifest.qualification_id ||
  authorization.status !== "CONSUMED" ||
  !isFullSha(authorization.pre_registration_commit) ||
  authorization.manifest_sha256 !== sha256(manifestBytes) ||
  authorization.authorization.maximum_subscription_invocations !== 100 ||
  authorization.authorization.approved_incremental_spend_usd !== 0 ||
  authorization.authorization.api_or_api_key_use_permitted !== false ||
  authorization.authorization.codex_access !==
    "Codex CLI through ChatGPT Pro" ||
  authorization.authorization.claude_access !==
    "Claude Code through Claude Max" ||
  authorization.execution.one_shot !== true ||
  authorization.execution.resume_or_replay_permitted !== false ||
  authorization.execution.attestation_ref !==
    "refs/distill/qualifications/us018-v2-20260724"
) {
  blockers.push("v2 pre-registration or authorization ledger is invalid");
}
for (const entry of authorization.historical_evidence) {
  const digest = sha256(await readFile(join(root, entry.path)));
  if (digest !== entry.sha256) {
    blockers.push(`historical evidence changed: ${entry.path}`);
  }
}

const expectedInvocationPairs = manifest.tasks.flatMap((task) => {
  const conditions =
    task.invocation_order === "raw_first"
      ? ["raw", "projected"]
      : ["projected", "raw"];
  return conditions.map((condition) => ({
    task_id: task.id,
    condition,
    provider: task.provider,
    requested_model: manifest.providers[task.provider].model,
  }));
});
const actualInvocationPairs = executionLedger.invocations.map((entry) => ({
  task_id: entry.task_id,
  condition: entry.condition,
  provider: entry.provider,
  requested_model: entry.requested_model,
}));
const executionLedgerValid =
  executionLedger.schema_version === "distill.paired-execution-ledger/v2" &&
  executionLedger.qualification_id === manifest.qualification_id &&
  executionLedger.status === "COMPLETE" &&
  executionLedger.source_worktree_clean === true &&
  executionLedger.maximum_invocations === 100 &&
  executionLedger.attestation?.ref ===
    authorization.execution.attestation_ref &&
  isFullSha(executionLedger.attestation?.start_commit) &&
  isFullSha(executionLedger.attestation?.head_commit) &&
  executionLedger.invocations.length === 100 &&
  executionLedger.invocations.every(
    (entry, index) =>
      entry.sequence === index + 1 &&
      entry.outcome !== "STARTED" &&
      typeof entry.started_at === "string" &&
      typeof entry.completed_at === "string",
  ) &&
  JSON.stringify(actualInvocationPairs) ===
    JSON.stringify(expectedInvocationPairs);
if (!executionLedgerValid) {
  blockers.push(
    "v2 execution ledger is incomplete, reordered, or inconsistent",
  );
}

const pairedEvidenceValid =
  paired.schema_version === "distill.paired-qualification/v2" &&
  paired.qualification_id === manifest.qualification_id &&
  paired.phase === "complete" &&
  paired.status === "GO" &&
  paired.evaluated_inputs?.source_worktree_clean === true &&
  isFullSha(paired.evaluated_inputs?.git_revision) &&
  isFullSha(paired.evaluated_inputs?.source_tree) &&
  paired.evaluated_inputs?.native_binary_sha256 === canonicalBinarySha256 &&
  paired.evaluated_inputs?.native_binary_sha256_after_preparation ===
    canonicalBinarySha256 &&
  paired.evaluated_inputs?.native_binary_sha256_after_execution ===
    canonicalBinarySha256 &&
  paired.evaluated_inputs?.canonical_build_command ===
    "cargo build --locked --release --manifest-path native/distill-core/Cargo.toml" &&
  paired.evaluated_inputs?.canonical_binary_path ===
    "native/distill-core/target/release/distill" &&
  paired.evaluated_inputs?.paired_task_manifest_sha256 ===
    sha256(manifestBytes) &&
  paired.evaluated_inputs?.authorization_ledger_sha256 ===
    sha256(authorizationBytes) &&
  paired.evaluated_inputs?.execution_ledger_sha256 ===
    sha256(await readFile(executionLedgerPath)) &&
  paired.billing?.api_keys_removed_from_child_environment === true &&
  paired.billing?.approved_incremental_spend_usd === 0 &&
  paired.billing?.actual_incremental_spend_usd === 0 &&
  paired.billing?.invocations === 100 &&
  paired.billing?.maximum_invocations === 100 &&
  JSON.stringify(paired.billing?.removed_environment_variables) ===
    JSON.stringify([
      "OPENAI_API_KEY",
      "OPENAI_BASE_URL",
      "ANTHROPIC_API_KEY",
      "ANTHROPIC_AUTH_TOKEN",
      "ANTHROPIC_BASE_URL",
      "CLAUDE_CODE_USE_BEDROCK",
      "CLAUDE_CODE_USE_VERTEX",
      "CLAUDE_CODE_USE_FOUNDRY",
    ]) &&
  paired.providers?.codex?.subscription === "ChatGPT Pro" &&
  paired.providers?.codex?.requested_model === manifest.providers.codex.model &&
  paired.providers?.claude?.subscription === "Claude Max" &&
  paired.providers?.claude?.requested_model ===
    manifest.providers.claude.model &&
  paired.authentication?.codex?.login_status === "Logged in using ChatGPT" &&
  paired.authentication?.codex?.access === "ChatGPT subscription" &&
  paired.authentication?.claude?.logged_in === true &&
  paired.authentication?.claude?.auth_method === "claude.ai" &&
  paired.authentication?.claude?.api_provider === "firstParty" &&
  paired.authentication?.claude?.subscription_type === "max" &&
  paired.authentication?.pii_fields_omitted === true &&
  paired.controls?.paired_prompt_model_tool_and_sampling_identity === true &&
  JSON.stringify(paired.controls?.provider_order_cells) ===
    JSON.stringify({
      "claude|projected_first": 13,
      "claude|raw_first": 12,
      "codex|projected_first": 12,
      "codex|raw_first": 13,
    }) &&
  paired.results?.length === 50;
if (!pairedEvidenceValid) {
  blockers.push(
    "v2 paired-task evidence is incomplete, inconsistent, or NO-GO",
  );
}

const manifestTasks = [...manifest.tasks].sort((left, right) =>
  left.id.localeCompare(right.id),
);
const reportedTasks = [...(paired.results ?? [])].sort((left, right) =>
  left.id.localeCompare(right.id),
);
const recomputedResults = reportedTasks.map((result, index) =>
  validateReportedTask(result, manifestTasks[index]),
);
const reportedTasksValid =
  recomputedResults.length === manifestTasks.length &&
  recomputedResults.every((result) => result.valid);
if (!reportedTasksValid) {
  blockers.push(
    "v2 task results do not match frozen prompts, schemas, or rubrics",
  );
}

const rawSuccesses = recomputedResults.filter(
  (result) => result.rawSuccess,
).length;
const projectedSuccesses = recomputedResults.filter(
  (result) => result.projectedSuccess,
).length;
const deltaPercentagePoints =
  ((projectedSuccesses - rawSuccesses) / manifest.sample_size) * 100;
const recomputedFailures = recomputedResults
  .filter((result) => !result.rawSuccess || !result.projectedSuccess)
  .map((result) => ({
    id: result.id,
    raw_success: result.rawSuccess,
    projected_success: result.projectedSuccess,
    raw_parse_error: result.rawParseError,
    projected_parse_error: result.projectedParseError,
    raw_mismatches: result.rawMismatches,
    projected_mismatches: result.projectedMismatches,
  }));
const aggregateEvidenceValid =
  rawSuccesses >= manifest.minimum_raw_successes &&
  passesDeltaGate(projectedSuccesses, rawSuccesses) &&
  paired.scoring?.sample_size === manifest.sample_size &&
  paired.scoring?.raw_successes === rawSuccesses &&
  paired.scoring?.projected_successes === projectedSuccesses &&
  nearlyEqual(
    paired.scoring?.raw_success_rate,
    rawSuccesses / manifest.sample_size,
  ) &&
  nearlyEqual(
    paired.scoring?.projected_success_rate,
    projectedSuccesses / manifest.sample_size,
  ) &&
  nearlyEqual(paired.scoring?.delta_percentage_points, deltaPercentagePoints) &&
  Array.isArray(paired.scoring?.confidence_interval_percentage_points) &&
  paired.scoring.confidence_interval_percentage_points.length === 2 &&
  JSON.stringify(paired.scoring?.failures) ===
    JSON.stringify(recomputedFailures) &&
  paired.token_counts?.raw_observation_tokens ===
    reportedTasks.reduce(
      (total, result) => total + result.observation_tokens.raw,
      0,
    ) &&
  paired.token_counts?.projected_observation_tokens ===
    reportedTasks.reduce(
      (total, result) => total + result.observation_tokens.projected,
      0,
    ) &&
  paired.token_counts?.provider_reported_usage?.codex?.length === 25 &&
  paired.token_counts?.provider_reported_usage?.claude?.length === 25;
if (!aggregateEvidenceValid) {
  blockers.push("v2 aggregate scoring or token accounting does not recompute");
}

const invocationOutcomesValid = expectedInvocationPairs.every(
  (expected, index) => {
    const ledgerEntry = executionLedger.invocations[index];
    const result = reportedTasks.find(
      (candidate) => candidate.id === expected.task_id,
    )?.[expected.condition];
    if (!ledgerEntry || !result) return false;
    return (
      ledgerEntry.outcome ===
        (result.provider_error ? "PROVIDER_ERROR" : "COMPLETED") &&
      ledgerEntry.prompt_sha256 === result.evidence?.prompt_sha256 &&
      ledgerEntry.observation_sha256 === result.evidence?.observation_sha256 &&
      ledgerEntry.raw_stdout_sha256 ===
        sha256(Buffer.from(result.raw_cli?.stdout ?? "", "utf8")) &&
      ledgerEntry.raw_stderr_sha256 ===
        sha256(Buffer.from(result.raw_cli?.stderr ?? "", "utf8")) &&
      ledgerEntry.response_sha256 ===
        (typeof result.response === "string"
          ? sha256(Buffer.from(result.response, "utf8"))
          : null) &&
      ledgerEntry.attestation_commit === result.evidence?.attestation_commit
    );
  },
);
if (!invocationOutcomesValid) {
  blockers.push(
    "v2 invocation outcomes do not match reported provider failures",
  );
}
const attestationChainValid = validateAttestationChain();
if (!attestationChainValid) {
  blockers.push(
    "v2 Git attestation chain does not bind all prompts, observations, and raw CLI outputs",
  );
}

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
  macos.surfaces?.project?.byte_exact_restore === true &&
  macos.status === "GO";
if (!macosEvidenceValid) {
  blockers.push("macOS arm64 evidence is incomplete or NO-GO");
}

const pairedRevision = paired.evaluated_inputs?.git_revision;
const sameCleanSourceRevision =
  isFullSha(pairedRevision) &&
  isFullSha(executionLedger.git_revision) &&
  isFullSha(macos.git_revision) &&
  commitExists(pairedRevision) &&
  pairedRevision === executionLedger.git_revision &&
  pairedRevision === macos.git_revision &&
  paired.evaluated_inputs?.source_worktree_clean === true &&
  executionLedger.source_worktree_clean === true &&
  macos.source_worktree_clean === true;
if (!sameCleanSourceRevision) {
  blockers.push(
    "paired and macOS evidence do not identify the same clean candidate revision",
  );
}
const authorizationConsumptionValid =
  sameCleanSourceRevision && validateAuthorizationConsumption(pairedRevision);
if (!authorizationConsumptionValid) {
  blockers.push(
    "v2 authorization was not consumed by the dedicated pushed candidate commit",
  );
}

const report = {
  schema_version: "distill.us018-qualification/v2",
  qualification_id: manifest.qualification_id,
  generated_at: new Date().toISOString(),
  paired: {
    status: paired.status,
    git_revision: paired.evaluated_inputs?.git_revision ?? null,
    report_sha256: sha256(await readFile(pairedPath)),
    execution_ledger_sha256: sha256(await readFile(executionLedgerPath)),
    sample_size: paired.scoring?.sample_size ?? null,
    raw_successes: paired.scoring?.raw_successes ?? null,
    projected_successes: paired.scoring?.projected_successes ?? null,
    delta_percentage_points: paired.scoring?.delta_percentage_points ?? null,
    invocations: paired.billing?.invocations ?? null,
    actual_incremental_spend_usd:
      paired.billing?.actual_incremental_spend_usd ?? null,
  },
  macos: {
    status: macos.status,
    git_revision: macos.git_revision,
    binary_sha256: macos.binary_sha256 ?? null,
    workflow_run_url: macos.workflow_run_url ?? null,
  },
  same_clean_source_revision: sameCleanSourceRevision,
  authorization_consumption_valid: authorizationConsumptionValid,
  paired_evidence_valid: pairedEvidenceValid,
  reported_tasks_valid: reportedTasksValid,
  aggregate_evidence_valid: aggregateEvidenceValid,
  execution_ledger_valid: executionLedgerValid,
  invocation_outcomes_valid: invocationOutcomesValid,
  attestation_chain_valid: attestationChainValid,
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

function validateReportedTask(result, task) {
  if (!result || !task) {
    return {
      id: result?.id ?? task?.id ?? null,
      valid: false,
      rawSuccess: false,
      projectedSuccess: false,
      rawParseError: "missing task",
      projectedParseError: "missing task",
      rawMismatches: [],
      projectedMismatches: [],
    };
  }
  const raw = recomputeScore(result.raw, task.rubric.expected);
  const projected = recomputeScore(result.projected, task.rubric.expected);
  const modelIdentity =
    result.raw?.requested_model === result.projected?.requested_model &&
    result.raw?.requested_model === manifest.providers[task.provider].model &&
    JSON.stringify(result.raw?.reported_models) ===
      JSON.stringify(result.projected?.reported_models);
  const frozenTask =
    result.id === task.id &&
    result.fixture_id === task.fixture_id &&
    result.category === task.category &&
    result.provider === task.provider &&
    result.invocation_order === task.invocation_order &&
    result.question === task.question &&
    result.response_schema === task.response_schema &&
    JSON.stringify(result.rubric) === JSON.stringify(task.rubric) &&
    result.observation_tokens?.budget === task.budget_tokens &&
    result.observation_tokens?.projected < result.observation_tokens?.raw;
  return {
    id: task.id,
    valid:
      frozenTask &&
      modelIdentity &&
      raw.matchesRecorded &&
      projected.matchesRecorded,
    rawSuccess: raw.success,
    projectedSuccess: projected.success,
    rawParseError: raw.parseError,
    projectedParseError: projected.parseError,
    rawMismatches: raw.mismatches,
    projectedMismatches: projected.mismatches,
  };
}

function recomputeScore(condition, expected) {
  let parsed = null;
  let parseError = condition?.provider_error ?? null;
  if (parseError === null) {
    try {
      parsed = JSON.parse(condition.response.trim());
    } catch (error) {
      parseError = error instanceof Error ? error.message : String(error);
    }
  }
  const expectedKeys = Object.keys(expected);
  const parsedKeys =
    parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? Object.keys(parsed)
      : [];
  const schemaMatches =
    parseError === null &&
    JSON.stringify(parsedKeys) === JSON.stringify(expectedKeys) &&
    parsedKeys.every((key) => typeof parsed[key] === "string");
  const mismatches = expectedKeys.filter(
    (key) => !schemaMatches || parsed[key] !== expected[key],
  );
  const success = schemaMatches && mismatches.length === 0;
  return {
    success,
    parseError,
    mismatches,
    matchesRecorded:
      condition?.success === success &&
      condition?.parse_error === parseError &&
      JSON.stringify(condition?.mismatched_fields) ===
        JSON.stringify(mismatches) &&
      JSON.stringify(condition?.parsed) === JSON.stringify(parsed),
  };
}

function passesDeltaGate(projectedSuccesses, rawSuccesses) {
  return (
    (projectedSuccesses - rawSuccesses) * 100 >=
    manifest.minimum_delta_percentage_points * manifest.sample_size
  );
}

function validateAuthorizationConsumption(revision) {
  const parent = safeCommand("git", [
    "-C",
    root,
    "show",
    "-s",
    "--format=%P",
    revision,
  ]);
  const changedFiles = safeCommand("git", [
    "-C",
    root,
    "diff-tree",
    "--no-commit-id",
    "--name-only",
    "-r",
    authorization.pre_registration_commit,
    revision,
  ]);
  const previousLedgerText = safeCommand("git", [
    "-C",
    root,
    "show",
    `${authorization.pre_registration_commit}:evaluation/release/paired-qualification-v2-ledger.json`,
  ]);
  const candidateLedgerText = safeCommand("git", [
    "-C",
    root,
    "show",
    `${revision}:evaluation/release/paired-qualification-v2-ledger.json`,
  ]);
  const candidateTree = safeCommand("git", [
    "-C",
    root,
    "rev-parse",
    `${revision}^{tree}`,
  ]);
  const ancestor = spawnSync("git", [
    "-C",
    root,
    "merge-base",
    "--is-ancestor",
    revision,
    "HEAD",
  ]);
  if (
    parent !== authorization.pre_registration_commit ||
    changedFiles !== "evaluation/release/paired-qualification-v2-ledger.json" ||
    !previousLedgerText ||
    !candidateLedgerText ||
    candidateTree !== paired.evaluated_inputs?.source_tree ||
    ancestor.status !== 0
  ) {
    return false;
  }
  try {
    const previousLedger = JSON.parse(previousLedgerText);
    const candidateLedger = JSON.parse(candidateLedgerText);
    return (
      previousLedger.status === "PRE_REGISTERED" &&
      previousLedger.manifest_sha256 === authorization.manifest_sha256 &&
      candidateLedger.status === "CONSUMED" &&
      candidateLedger.pre_registration_commit ===
        authorization.pre_registration_commit &&
      typeof candidateLedger.consumed_at === "string" &&
      JSON.stringify(candidateLedger) === JSON.stringify(authorization)
    );
  } catch {
    return false;
  }
}

function validateAttestationChain() {
  const revision = paired.evaluated_inputs?.git_revision;
  const startCommit = executionLedger.attestation?.start_commit;
  let previous = revision;
  const startPayload = {
    schema_version: "distill.paired-attestation/v2",
    qualification_id: manifest.qualification_id,
    type: "START",
    git_revision: revision,
    source_tree: paired.evaluated_inputs?.source_tree,
    manifest_sha256: sha256(manifestBytes),
    authorization_ledger_sha256: sha256(authorizationBytes),
    started_at: executionLedger.started_at,
  };
  if (!validateAttestationCommit(startCommit, previous, startPayload)) {
    return false;
  }
  previous = startCommit;
  for (const entry of executionLedger.invocations) {
    const payload = {
      schema_version: "distill.paired-attestation/v2",
      qualification_id: manifest.qualification_id,
      type: "INVOCATION",
      sequence: entry.sequence,
      task_id: entry.task_id,
      condition: entry.condition,
      provider: entry.provider,
      requested_model: entry.requested_model,
      prompt_sha256: entry.prompt_sha256,
      observation_sha256: entry.observation_sha256,
      raw_stdout_sha256: entry.raw_stdout_sha256,
      raw_stderr_sha256: entry.raw_stderr_sha256,
      response_sha256: entry.response_sha256,
      outcome: entry.outcome,
      completed_at: entry.completed_at,
    };
    if (
      entry.previous_attestation_commit !== previous ||
      !validateAttestationCommit(entry.attestation_commit, previous, payload)
    ) {
      return false;
    }
    previous = entry.attestation_commit;
  }
  const refHead = safeCommand("git", [
    "-C",
    root,
    "rev-parse",
    authorization.execution.attestation_ref,
  ]);
  return (
    previous === executionLedger.attestation?.head_commit &&
    previous === refHead
  );
}

function validateAttestationCommit(commit, parent, payload) {
  if (!isFullSha(commit) || !commitExists(commit)) return false;
  const actualParent = safeCommand("git", [
    "-C",
    root,
    "show",
    "-s",
    "--format=%P",
    commit,
  ]);
  const tree = safeCommand("git", [
    "-C",
    root,
    "show",
    "-s",
    "--format=%T",
    commit,
  ]);
  const message = safeCommand("git", [
    "-C",
    root,
    "show",
    "-s",
    "--format=%B",
    commit,
  ]);
  if (
    actualParent !== parent ||
    tree !== paired.evaluated_inputs?.source_tree ||
    !message
  ) {
    return false;
  }
  try {
    return JSON.stringify(JSON.parse(message)) === JSON.stringify(payload);
  } catch {
    return false;
  }
}

function commitExists(revision) {
  if (!isFullSha(revision)) return false;
  return (
    spawnSync("git", ["-C", root, "cat-file", "-e", `${revision}^{commit}`])
      .status === 0
  );
}

function safeCommand(command, args) {
  const result = spawnSync(command, args, { encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : null;
}

function isFullSha(value) {
  return typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
}

function nearlyEqual(left, right) {
  return (
    typeof left === "number" &&
    typeof right === "number" &&
    Math.abs(left - right) < 1e-12
  );
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}
