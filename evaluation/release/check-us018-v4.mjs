import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const releaseRoot = join(root, "evaluation/release");
const evidenceRoot = join(releaseRoot, "evidence");
const manifestBytes = await readFile(join(releaseRoot, "paired-tasks-v4.json"));
const manifest = JSON.parse(manifestBytes);
const authorizationPath = join(
  releaseRoot,
  "paired-qualification-v4-ledger.json",
);
const authorizationBytes = await readFile(authorizationPath);
const authorization = JSON.parse(authorizationBytes);
const pairedPath = join(evidenceRoot, "paired-tasks-v4.json");
const executionLedgerPath = join(
  evidenceRoot,
  "paired-tasks-v4-execution-ledger.json",
);
const macosPath = join(evidenceRoot, "macos-arm64-v4.json");
const paired = await readJson(pairedPath);
const executionLedger = await readJson(executionLedgerPath);
const macos = await readJson(macosPath);
const macosReceiptSha256 = sha256(await readFile(macosPath));
const output = join(evidenceRoot, "us018-qualification-v4.json");
const canonicalBinary = join(
  root,
  "native/distill-core/target/release/distill",
);
const canonicalBinarySha256 = sha256(await readFile(canonicalBinary));

const blockers = [];
if (
  authorization.schema_version !== "distill.paired-qualification-ledger/v4" ||
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
  authorization.authorization.claude_model !== "claude-fable-5" ||
  authorization.authorization.claude_prompt_suggestions !== false ||
  authorization.authorization.fallback_api_permitted !== false ||
  authorization.execution.one_shot !== true ||
  authorization.execution.resume_or_replay_permitted !== false ||
  authorization.execution.no_retry !== true ||
  authorization.execution.stop_on_first_model_identity_divergence !== true ||
  authorization.execution.stop_on_first_required_primary_model_absence !==
    true ||
  authorization.execution.attestation_ref !==
    "refs/distill/qualifications/us018-v4-20260724"
) {
  blockers.push("v4 pre-registration or authorization ledger is invalid");
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
const executionStoppedEarly =
  executionLedger.status === "EARLY_STOPPED" &&
  [
    "REPORTED_MODEL_IDENTITY_DIVERGENCE",
    "REQUIRED_PRIMARY_MODEL_NOT_REPORTED",
  ].includes(executionLedger.terminal?.reason);
const executionCompleted =
  executionLedger.status === "COMPLETE" &&
  executionLedger.invocations.length === manifest.maximum_invocations &&
  executionLedger.terminal === null;
const executionLedgerValid =
  executionLedger.schema_version === "distill.paired-execution-ledger/v4" &&
  executionLedger.qualification_id === manifest.qualification_id &&
  (executionCompleted || executionStoppedEarly) &&
  executionLedger.source_worktree_clean === true &&
  executionLedger.maximum_invocations === 100 &&
  executionLedger.attestation?.ref ===
    authorization.execution.attestation_ref &&
  isFullSha(executionLedger.attestation?.start_commit) &&
  isFullSha(executionLedger.attestation?.head_commit) &&
  executionLedger.invocations.length > 0 &&
  executionLedger.invocations.length <= 100 &&
  executionLedger.invocations.length % 2 === 0 &&
  executionLedger.invocations.every(
    (entry, index) =>
      entry.sequence === index + 1 &&
      entry.outcome !== "STARTED" &&
      typeof entry.started_at === "string" &&
      typeof entry.completed_at === "string",
  ) &&
  JSON.stringify(actualInvocationPairs) ===
    JSON.stringify(
      expectedInvocationPairs.slice(0, executionLedger.invocations.length),
    );
if (!executionLedgerValid) {
  blockers.push(
    "v4 execution ledger is incomplete, reordered, or inconsistent",
  );
}

const pairedEvidenceValid =
  paired.schema_version === "distill.paired-qualification/v4" &&
  paired.qualification_id === manifest.qualification_id &&
  paired.phase === "complete" &&
  ["GO", "NO-GO"].includes(paired.status) &&
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
  paired.billing?.invocations === executionLedger.invocations.length &&
  paired.billing?.invocations > 0 &&
  paired.billing?.invocations <= 100 &&
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
  paired.providers?.claude?.sampling_parameters?.prompt_suggestions === false &&
  paired.authentication?.codex?.login_status === "Logged in using ChatGPT" &&
  paired.authentication?.codex?.access === "ChatGPT subscription" &&
  paired.authentication?.claude?.logged_in === true &&
  paired.authentication?.claude?.auth_method === "claude.ai" &&
  paired.authentication?.claude?.api_provider === "firstParty" &&
  paired.authentication?.claude?.subscription_type === "max" &&
  paired.authentication?.pii_fields_omitted === true &&
  paired.controls?.no_retry === true &&
  paired.controls?.stop_on_first_reported_model_identity_divergence === true &&
  paired.controls?.stop_on_first_required_primary_model_absence === true &&
  JSON.stringify(paired.controls?.provider_order_cells) ===
    JSON.stringify({
      "claude|projected_first": 13,
      "claude|raw_first": 12,
      "codex|projected_first": 12,
      "codex|raw_first": 13,
    }) &&
  paired.results?.length === executionLedger.invocations.length / 2 &&
  JSON.stringify(paired.termination) ===
    JSON.stringify(executionLedger.terminal);
if (!pairedEvidenceValid) {
  blockers.push("v4 paired-task evidence is incomplete or inconsistent");
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
  recomputedResults.length === reportedTasks.length &&
  recomputedResults.every((result) => result.contractValid);
if (!reportedTasksValid) {
  blockers.push(
    "v4 task results do not match frozen prompts, schemas, or rubrics",
  );
}

const rawSuccesses = recomputedResults.filter(
  (result) => result.rawSuccess,
).length;
const projectedSuccesses = recomputedResults.filter(
  (result) => result.projectedSuccess,
).length;
const evaluatedSampleSize = recomputedResults.length;
const deltaPercentagePoints =
  evaluatedSampleSize === 0
    ? 0
    : ((projectedSuccesses - rawSuccesses) / evaluatedSampleSize) * 100;
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
  paired.scoring?.sample_size === evaluatedSampleSize &&
  paired.scoring?.raw_successes === rawSuccesses &&
  paired.scoring?.projected_successes === projectedSuccesses &&
  nearlyEqual(
    paired.scoring?.raw_success_rate,
    evaluatedSampleSize === 0 ? 0 : rawSuccesses / evaluatedSampleSize,
  ) &&
  nearlyEqual(
    paired.scoring?.projected_success_rate,
    evaluatedSampleSize === 0 ? 0 : projectedSuccesses / evaluatedSampleSize,
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
  paired.token_counts?.provider_reported_usage?.codex?.length ===
    reportedTasks.filter((result) => result.provider === "codex").length &&
  paired.token_counts?.provider_reported_usage?.claude?.length ===
    reportedTasks.filter((result) => result.provider === "claude").length;
if (!aggregateEvidenceValid) {
  blockers.push("v4 aggregate scoring or token accounting does not recompute");
}

const invocationOutcomesValid = expectedInvocationPairs
  .slice(0, executionLedger.invocations.length)
  .every((expected, index) => {
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
      ledgerEntry.reported_models_sha256 ===
        sha256(Buffer.from(JSON.stringify(result.reported_models), "utf8")) &&
      ledgerEntry.attestation_commit === result.evidence?.attestation_commit
    );
  });
if (!invocationOutcomesValid) {
  blockers.push(
    "v4 invocation outcomes do not match reported provider failures",
  );
}
const firstModelDivergenceIndex = recomputedResults.findIndex(
  (result) => !result.modelIdentity,
);
const firstModelDivergence =
  firstModelDivergenceIndex === -1
    ? null
    : reportedTasks[firstModelDivergenceIndex];
const expectedTerminalReason =
  firstModelDivergence &&
  JSON.stringify(firstModelDivergence.raw?.reported_models) ===
    JSON.stringify(firstModelDivergence.projected?.reported_models)
    ? "REQUIRED_PRIMARY_MODEL_NOT_REPORTED"
    : "REPORTED_MODEL_IDENTITY_DIVERGENCE";
const earlyStopMechanicallyValid = executionStoppedEarly
  ? firstModelDivergenceIndex === reportedTasks.length - 1 &&
    recomputedResults
      .slice(0, firstModelDivergenceIndex)
      .every((result) => result.modelIdentity) &&
    executionLedger.terminal?.task_id === firstModelDivergence?.id &&
    executionLedger.terminal?.provider === firstModelDivergence?.provider &&
    executionLedger.terminal?.reason === expectedTerminalReason &&
    executionLedger.terminal?.invocations_consumed ===
      executionLedger.invocations.length &&
    executionLedger.terminal?.retry_permitted === false &&
    JSON.stringify(executionLedger.terminal?.raw_reported_models) ===
      JSON.stringify(firstModelDivergence?.raw?.reported_models) &&
    JSON.stringify(executionLedger.terminal?.projected_reported_models) ===
      JSON.stringify(firstModelDivergence?.projected?.reported_models)
  : executionCompleted && firstModelDivergenceIndex === -1;
if (!earlyStopMechanicallyValid) {
  blockers.push(
    "v4 terminal state does not enforce the first reported-model divergence",
  );
}
const pairedGateGo =
  executionCompleted &&
  reportedTasks.length === manifest.sample_size &&
  rawSuccesses >= manifest.minimum_raw_successes &&
  passesDeltaGate(projectedSuccesses, rawSuccesses) &&
  firstModelDivergenceIndex === -1 &&
  paired.status === "GO";
const firstDefect = executionStoppedEarly
  ? {
      reason: executionLedger.terminal.reason,
      task_id: executionLedger.terminal.task_id,
      provider: executionLedger.terminal.provider,
      raw_reported_models: executionLedger.terminal.raw_reported_models,
      projected_reported_models:
        executionLedger.terminal.projected_reported_models,
      invocations_consumed: executionLedger.terminal.invocations_consumed,
    }
  : null;
if (!pairedGateGo) {
  blockers.unshift(
    firstDefect
      ? `v4 stopped at ${firstDefect.task_id}: ${firstDefect.reason}`
      : "v4 paired-task gate is NO-GO",
  );
}
const attestationChainValid = validateAttestationChain();
if (!attestationChainValid) {
  blockers.push(
    "v4 Git attestation chain does not bind all prompts, observations, and raw CLI outputs",
  );
}
const remoteAttestation =
  safeCommand("git", [
    "-C",
    root,
    "ls-remote",
    "origin",
    authorization.execution.attestation_ref,
  ]) ?? "";
const remoteAttestationValid =
  remoteAttestation ===
  `${executionLedger.attestation?.head_commit}\t${authorization.execution.attestation_ref}`;
if (!remoteAttestationValid) {
  blockers.push("v4 attestation ref is not durably published to origin");
}

const macosWorkflowVerification = await verifyMacosWorkflowReceipt(
  macos,
  macosReceiptSha256,
);
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
  macos.status === "GO" &&
  macosWorkflowVerification.valid;
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
    "v4 authorization was not consumed by the dedicated pushed candidate commit",
  );
}

const report = {
  schema_version: "distill.us018-qualification/v4",
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
    workflow_verification: macosWorkflowVerification,
  },
  same_clean_source_revision: sameCleanSourceRevision,
  authorization_consumption_valid: authorizationConsumptionValid,
  paired_evidence_valid: pairedEvidenceValid,
  reported_tasks_valid: reportedTasksValid,
  aggregate_evidence_valid: aggregateEvidenceValid,
  execution_ledger_valid: executionLedgerValid,
  invocation_outcomes_valid: invocationOutcomesValid,
  early_stop_mechanically_valid: earlyStopMechanicallyValid,
  paired_gate_go: pairedGateGo,
  attestation_chain_valid: attestationChainValid,
  remote_attestation_valid: remoteAttestationValid,
  macos_evidence_valid: macosEvidenceValid,
  first_defect: firstDefect,
  blockers,
  status:
    blockers.length === 0 && pairedGateGo && macosEvidenceValid
      ? "GO"
      : "NO-GO",
};
await mkdir(dirname(output), { recursive: true });
await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
if (report.status !== "GO") {
  process.exitCode = 1;
}

async function verifyMacosWorkflowReceipt(receipt, expectedReceiptSha256) {
  const match =
    typeof receipt.workflow_run_url === "string"
      ? receipt.workflow_run_url.match(
          /^https:\/\/github\.com\/arthjean\/distill\/actions\/runs\/(\d+)$/,
        )
      : null;
  if (!match) {
    return { valid: false, error: "invalid workflow run URL" };
  }
  const runId = match[1];
  const view = spawnSync(
    "gh",
    [
      "run",
      "view",
      runId,
      "--repo",
      "arthjean/distill",
      "--json",
      "conclusion,event,headSha,url,workflowName",
    ],
    { encoding: "utf8" },
  );
  if (view.status !== 0) {
    return {
      valid: false,
      run_id: runId,
      error: `gh run view failed: ${view.stderr.trim().slice(0, 500)}`,
    };
  }
  let run;
  try {
    run = JSON.parse(view.stdout);
  } catch {
    return { valid: false, run_id: runId, error: "invalid gh run metadata" };
  }
  const downloadDirectory = await mkdtemp(
    join(tmpdir(), "distill-us018-v4-macos-verification-"),
  );
  try {
    const download = spawnSync(
      "gh",
      [
        "run",
        "download",
        runId,
        "--repo",
        "arthjean/distill",
        "--name",
        "distill-macos-arm64-qualification",
        "--dir",
        downloadDirectory,
      ],
      { encoding: "utf8" },
    );
    if (download.status !== 0) {
      return {
        valid: false,
        run_id: runId,
        head_sha: run.headSha ?? null,
        error: `gh run download failed: ${download.stderr.trim().slice(0, 500)}`,
      };
    }
    const downloadedReceiptSha256 = sha256(
      await readFile(join(downloadDirectory, "macos-arm64.json")),
    );
    return {
      valid:
        run.conclusion === "success" &&
        run.event === "push" &&
        run.headSha === receipt.git_revision &&
        run.url === receipt.workflow_run_url &&
        run.workflowName === "Native macOS arm64 qualification" &&
        downloadedReceiptSha256 === expectedReceiptSha256,
      run_id: runId,
      conclusion: run.conclusion ?? null,
      event: run.event ?? null,
      head_sha: run.headSha ?? null,
      workflow_name: run.workflowName ?? null,
      downloaded_receipt_sha256: downloadedReceiptSha256,
      local_receipt_sha256: expectedReceiptSha256,
    };
  } finally {
    await rm(downloadDirectory, { recursive: true, force: true });
  }
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf8"));
}

function validateReportedTask(result, task) {
  if (!result || !task) {
    return {
      id: result?.id ?? task?.id ?? null,
      contractValid: false,
      modelIdentity: false,
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
  const pairReportedModelsEqual =
    JSON.stringify(result.raw?.reported_models) ===
    JSON.stringify(result.projected?.reported_models);
  const reportedModelContractSatisfied =
    task.provider === "claude"
      ? result.raw?.reported_models?.some(isRequiredClaudePrimary) === true &&
        result.projected?.reported_models?.some(isRequiredClaudePrimary) ===
          true
      : pairReportedModelsEqual &&
        (result.raw?.reported_models?.length === 0 ||
          JSON.stringify(result.raw?.reported_models) ===
            JSON.stringify([manifest.providers.codex.model]));
  const modelIdentity =
    result.raw?.requested_model === result.projected?.requested_model &&
    result.raw?.requested_model === manifest.providers[task.provider].model &&
    pairReportedModelsEqual &&
    reportedModelContractSatisfied;
  const modelIdentityControlMatches =
    result.model_identity_control?.matches === modelIdentity &&
    result.model_identity_control?.expected_requested_model ===
      manifest.providers[task.provider].model &&
    result.model_identity_control?.pair_reported_models_equal ===
      pairReportedModelsEqual &&
    result.model_identity_control?.reported_model_contract_satisfied ===
      reportedModelContractSatisfied &&
    result.model_identity_control?.raw_requested_model ===
      result.raw?.requested_model &&
    result.model_identity_control?.projected_requested_model ===
      result.projected?.requested_model &&
    JSON.stringify(result.model_identity_control?.raw_reported_models) ===
      JSON.stringify(result.raw?.reported_models) &&
    JSON.stringify(result.model_identity_control?.projected_reported_models) ===
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
    contractValid:
      frozenTask &&
      modelIdentityControlMatches &&
      raw.matchesRecorded &&
      projected.matchesRecorded,
    modelIdentity,
    rawSuccess: raw.success,
    projectedSuccess: projected.success,
    rawParseError: raw.parseError,
    projectedParseError: projected.parseError,
    rawMismatches: raw.mismatches,
    projectedMismatches: projected.mismatches,
  };
}

function isRequiredClaudePrimary(model) {
  return (
    model?.id === "claude-fable-5" &&
    model?.canonical_model === "claude-fable-5" &&
    model?.provider === "firstParty"
  );
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
    `${authorization.pre_registration_commit}:evaluation/release/paired-qualification-v4-ledger.json`,
  ]);
  const candidateLedgerText = safeCommand("git", [
    "-C",
    root,
    "show",
    `${revision}:evaluation/release/paired-qualification-v4-ledger.json`,
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
    changedFiles !== "evaluation/release/paired-qualification-v4-ledger.json" ||
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
      previousLedger.pre_registration_commit === null &&
      previousLedger.consumed_at === null &&
      candidateLedger.status === "CONSUMED" &&
      candidateLedger.pre_registration_commit ===
        authorization.pre_registration_commit &&
      typeof candidateLedger.consumed_at === "string" &&
      JSON.stringify(candidateLedger) === JSON.stringify(authorization) &&
      JSON.stringify(previousLedger) ===
        JSON.stringify({
          ...candidateLedger,
          status: "PRE_REGISTERED",
          pre_registration_commit: null,
          consumed_at: null,
        })
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
    schema_version: "distill.paired-attestation/v4",
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
      schema_version: "distill.paired-attestation/v4",
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
      reported_models_sha256: entry.reported_models_sha256,
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
