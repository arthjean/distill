import { createHash } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { cpus, platform, arch, release, totalmem, tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { autoOptimizeTool } from "../../packages/mcp-server/src/tools/auto-optimize.js";
import { executeSmartFileRead } from "../../packages/mcp-server/src/tools/smart-file-read.js";
import {
  getOriginStore,
  RETRIEVE_ENV_VAR,
} from "../../packages/mcp-server/src/retrieve/origin-store.js";
import { countTokens } from "../../packages/mcp-server/src/utils/token-counter.js";
import {
  isValidUtf8,
  loadProfiles,
  materializeSource,
  percentile,
  readManifest,
  scoreProjection,
  sha256,
  validateCorpus,
} from "../corpus/lib.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const legacyDirectory = dirname(scriptPath);
const root = join(legacyDirectory, "..", "..");
const corpusDirectory = join(root, "evaluation", "corpus");
const manifestPath = join(corpusDirectory, "manifest.jsonl");
const profilesPath = join(corpusDirectory, "budget-profiles.json");
const evidencePath = join(legacyDirectory, "evidence.json");
const reportPath = join(legacyDirectory, "baseline.md");
const sourceRoot = join(root, "packages", "mcp-server", "src");
const WARMUP_RUNS = 2;
const MEASURED_RUNS = 10;
const LARGE_FIXTURE_TIMEOUT_MS = 15_000;
const LARGE_TIMING_TIMEOUT_MS = 5_000;

if (process.argv[2] === "--probe-origin") {
  const handle = process.argv[3] ?? "";
  process.stdout.write(getOriginStore().get(handle) === undefined ? "missing\n" : "present\n");
  process.exit(0);
}

if (process.argv[2] === "--probe-auto-fixture") {
  delete process.env.DISTILL_COMPRESSED_MARKERS;
  delete process.env[RETRIEVE_ENV_VAR];
  const fixtureId = process.argv[3] ?? "";
  const probeFixtures = await readManifest(manifestPath);
  const probeFixture = probeFixtures.find((fixture) => fixture.id === fixtureId);
  if (!probeFixture) {
    throw new Error(`unknown probe fixture: ${fixtureId}`);
  }
  process.stdout.write(JSON.stringify(await evaluateAutoFixtureDirect(probeFixture)));
  process.exit(0);
}

if (process.argv[2] === "--probe-auto-output") {
  delete process.env.DISTILL_COMPRESSED_MARKERS;
  delete process.env[RETRIEVE_ENV_VAR];
  const fixtureId = process.argv[3] ?? "";
  const probeFixtures = await readManifest(manifestPath);
  const probeFixture = probeFixtures.find((fixture) => fixture.id === fixtureId);
  if (!probeFixture) {
    throw new Error(`unknown output probe fixture: ${fixtureId}`);
  }
  const input = materializeSource(probeFixture.source).toString("utf8");
  const visible = outputText(
    await autoOptimizeTool.execute({
      content: input,
      strategy: "auto",
      response_format: "minimal",
    }),
  );
  process.stdout.write(JSON.stringify({ output_sha256: sha256(Buffer.from(visible)) }));
  process.exit(0);
}

const mode = process.argv[2];
if (mode !== "--write" && mode !== "--verify") {
  throw new Error("usage: bun evaluation/legacy/run.mjs --write|--verify");
}

const priorMarkers = process.env.DISTILL_COMPRESSED_MARKERS;
const priorRetrieve = process.env[RETRIEVE_ENV_VAR];
delete process.env.DISTILL_COMPRESSED_MARKERS;
delete process.env[RETRIEVE_ENV_VAR];

try {
  if (mode === "--write") {
    await writeBaseline();
  } else {
    await verifyBaseline();
  }
} finally {
  restoreEnvironment("DISTILL_COMPRESSED_MARKERS", priorMarkers);
  restoreEnvironment(RETRIEVE_ENV_VAR, priorRetrieve);
  getOriginStore().clear();
}

async function writeBaseline() {
  assertLegacySourceIsClean();
  const profiles = await loadProfiles(profilesPath);
  const fixtures = await readManifest(manifestPath);
  const corpusSummary = validateCorpus(fixtures, profiles);
  const fixtureById = new Map(fixtures.map((fixture) => [fixture.id, fixture]));
  const temporaryRoot = await mkdtemp(join(tmpdir(), "distill-legacy-baseline-"));

  try {
    await materializeFileFixtures(fixtures, temporaryRoot);
    const autoCases = [];
    for (const fixture of fixtures) {
      autoCases.push(await evaluateAutoFixture(fixture));
    }

    const fileFixtures = fixtures.filter((fixture) => fixture.source.kind === "file");
    const smartCases = [];
    for (const fixture of fileFixtures) {
      smartCases.push(await evaluateSmartFixture(fixture, temporaryRoot));
    }

    const timingCases = [
      await timeAutoFixture(fixtureById.get("build-output-01")),
      await timeAutoFixture(fixtureById.get("diff-01")),
      await timeAutoFixture(fixtureById.get("unicode-01")),
      await timeAutoFixture(fixtureById.get("boundary-one-mib")),
      await timeSmartFixture(
        fixtureById.get("source-code-01"),
        temporaryRoot,
      ),
      await timeSmartFixture(fixtureById.get("json-01"), temporaryRoot),
    ];

    const recovery = await measureProcessScopedRecovery(
      fixtureById.get("build-output-01"),
    );
    timingCases.push(recovery.timing);

    const autoSummary = aggregateCases(autoCases);
    const smartSummary = aggregateCases(smartCases);
    const sourceFiles = await listFiles(sourceRoot, (path) => path.endsWith(".ts"));
    const baselineCommit = git(["rev-parse", "HEAD"]).trim();
    const generatedAt = new Date().toISOString();
    const corpusBytes = await readFile(manifestPath);
    const loc = await countLegacyLoc(sourceFiles);
    const codex = await inspectCodexInterception(sourceFiles);

    const evidence = {
      schema_version: "distill.legacy-baseline/v1",
      generated_at: generatedAt,
      baseline_git_commit: baselineCommit,
      worktree_paths_at_measurement: git(["status", "--porcelain=v1"])
        .split("\n")
        .filter(Boolean),
      legacy_source_sha256: await digestFiles(sourceFiles),
      reference_machine: referenceMachine(),
      protocol: {
        warmup_runs: WARMUP_RUNS,
        measured_runs: MEASURED_RUNS,
        clock: "performance.now",
        latency_unit: "milliseconds",
        rss_measurement: "process.memoryUsage().rss sampled after each measured run",
        token_profile: "cl100k_base via js-tiktoken 1.0.15 gpt-4 mapping",
      },
      corpus: {
        schema_version: "distill.projection-fixture/v1",
        manifest_sha256: sha256(corpusBytes),
        ...corpusSummary,
      },
      paths: {
        auto_optimize: {
          supported: true,
          cases: autoCases,
          summary: autoSummary,
        },
        smart_file_read: {
          supported: true,
          cases: smartCases,
          summary: smartSummary,
        },
        process_scoped_recovery: recovery.result,
        codex_interception: codex,
      },
      timing_cases: timingCases,
      invalid_cases: collectInvalidCases(autoCases, smartCases, timingCases),
      loc,
      salvage_matrix: buildSalvageMatrix(autoSummary, smartSummary, recovery.result),
    };

    const report = renderReport(evidence);
    await writeFile(evidencePath, `${JSON.stringify(evidence, null, 2)}\n`, {
      encoding: "utf8",
      mode: 0o600,
    });
    await writeFile(reportPath, report, { encoding: "utf8", mode: 0o600 });

    console.log(
      JSON.stringify({
        mode: "write",
        corpus_fixtures: fixtures.length,
        auto_cases: autoCases.length,
        smart_file_cases: smartCases.length,
        timing_cases: timingCases.length,
        invalid_cases: evidence.invalid_cases.length,
        baseline_git_commit: baselineCommit,
      }),
    );
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
}

async function verifyBaseline() {
  assertLegacySourceIsClean();
  const evidence = JSON.parse(await readFile(evidencePath, "utf8"));
  if (evidence.schema_version !== "distill.legacy-baseline/v1") {
    throw new Error("legacy evidence has an unsupported schema version");
  }
  if (!Number.isFinite(Date.parse(evidence.generated_at))) {
    throw new Error("legacy evidence generated_at is invalid");
  }
  if (evidence.protocol.warmup_runs < 2 || evidence.protocol.measured_runs < 10) {
    throw new Error("legacy evidence does not satisfy the warm-up and run protocol");
  }

  const profiles = await loadProfiles(profilesPath);
  const fixtures = await readManifest(manifestPath);
  validateCorpus(fixtures, profiles);
  const fixtureIds = new Set(fixtures.map((fixture) => fixture.id));
  const fileFixtureIds = new Set(
    fixtures.filter((fixture) => fixture.source.kind === "file").map((fixture) => fixture.id),
  );

  assertCaseCoverage(
    evidence.paths.auto_optimize.cases,
    fixtureIds,
    "auto_optimize",
  );
  assertCaseCoverage(
    evidence.paths.smart_file_read.cases,
    fileFixtureIds,
    "smart_file_read",
  );
  for (const candidate of [
    ...evidence.paths.auto_optimize.cases,
    ...evidence.paths.smart_file_read.cases,
  ]) {
    assertMeasuredCase(candidate);
  }
  for (const timing of evidence.timing_cases) {
    assertTimingCase(timing, evidence.protocol);
  }

  const sourceFiles = await listFiles(sourceRoot, (path) => path.endsWith(".ts"));
  const currentSourceDigest = await digestFiles(sourceFiles);
  if (currentSourceDigest !== evidence.legacy_source_sha256) {
    throw new Error("legacy production source differs from the measured baseline");
  }
  git(["rev-parse", "--verify", `${evidence.baseline_git_commit}^{commit}`]);
  const corpusDigest = sha256(await readFile(manifestPath));
  if (corpusDigest !== evidence.corpus.manifest_sha256) {
    throw new Error("corpus manifest differs from the measured baseline");
  }
  const currentLoc = await countLegacyLoc(sourceFiles);
  if (JSON.stringify(currentLoc) !== JSON.stringify(evidence.loc)) {
    throw new Error("legacy LOC context differs from the measured baseline");
  }
  if (
    evidence.paths.process_scoped_recovery.same_process_sha256_equal !== true ||
    evidence.paths.process_scoped_recovery.fresh_process_recovery !== false ||
    evidence.paths.codex_interception.supported !== false
  ) {
    throw new Error("legacy recovery or Codex interception evidence is incomplete");
  }

  const temporaryRoot = await mkdtemp(join(tmpdir(), "distill-legacy-verify-"));
  try {
    await materializeFileFixtures(fixtures, temporaryRoot);
    await replayStableAutoCase(
      fixtures.find((fixture) => fixture.id === "build-output-01"),
      evidence.paths.auto_optimize.cases,
    );
    await replayStableSmartCase(
      fixtures.find((fixture) => fixture.id === "source-code-01"),
      evidence.paths.smart_file_read.cases,
      temporaryRoot,
    );
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }

  const expectedReport = renderReport(evidence);
  const committedReport = await readFile(reportPath, "utf8");
  if (committedReport !== expectedReport) {
    throw new Error("baseline report does not match raw evidence");
  }

  console.log(
    JSON.stringify({
      mode: "verify",
      baseline_git_commit: evidence.baseline_git_commit,
      corpus_fixtures: fixtures.length,
      timing_cases: evidence.timing_cases.length,
      report: "consistent",
      stable_replays: 2,
    }),
  );
}

async function evaluateAutoFixture(fixture) {
  if (materializeSource(fixture.source).length >= 1024 * 1024) {
    return evaluateAutoFixtureIsolated(fixture);
  }
  return evaluateAutoFixtureDirect(fixture);
}

async function evaluateAutoFixtureIsolated(fixture) {
  const sourceBytes = materializeSource(fixture.source);
  const started = performance.now();
  const child = spawnSync(
    process.execPath,
    [scriptPath, "--probe-auto-fixture", fixture.id],
    {
      cwd: root,
      encoding: "utf8",
      timeout: LARGE_FIXTURE_TIMEOUT_MS,
      killSignal: "SIGKILL",
      maxBuffer: 2 * 1024 * 1024,
      env: {
        ...process.env,
        DISTILL_COMPRESSED_MARKERS: "",
        [RETRIEVE_ENV_VAR]: "",
      },
    },
  );
  const elapsed = performance.now() - started;
  if (child.status === 0) {
    return JSON.parse(child.stdout);
  }
  return {
    fixture_id: fixture.id,
    path: "auto_optimize",
    status: "invalid",
    input_bytes: sourceBytes.length,
    input_tokens: null,
    visible_bytes: null,
    visible_tokens: null,
    p0_declared: fixture.annotations.p0.length,
    p0_recalled: null,
    p1_declared: fixture.annotations.p1.length,
    p1_recalled: null,
    deterministic: null,
    output_sha256: null,
    latency_ms_single_observation: round(elapsed),
    rss_before_bytes: null,
    rss_after_bytes: null,
    method: null,
    tool_error: null,
    failure_mode:
      child.error?.code === "ETIMEDOUT" ? "measurement_timeout" : "measurement_error",
    error: {
      name: child.error?.code === "ETIMEDOUT" ? "MeasurementTimeout" : "ChildProcessError",
      message:
        child.error?.code === "ETIMEDOUT"
          ? `legacy projection exceeded ${LARGE_FIXTURE_TIMEOUT_MS} ms`
          : (child.stderr.trim() || child.error?.message || "isolated probe failed").slice(
              0,
              500,
            ),
      timeout_ms: LARGE_FIXTURE_TIMEOUT_MS,
      elapsed_ms: round(elapsed),
      child_status: child.status,
      child_signal: child.signal,
    },
    metadata_loss: [],
  };
}

async function evaluateAutoFixtureDirect(fixture) {
  const sourceBytes = materializeSource(fixture.source);
  if (!isValidUtf8(sourceBytes)) {
    return unsupportedCase(
      fixture,
      "auto_optimize",
      "requires_utf8_string",
      sourceBytes.length,
    );
  }

  const input = sourceBytes.toString("utf8");
  const rssBefore = process.memoryUsage().rss;
  const started = performance.now();
  try {
    const first = await autoOptimizeTool.execute({
      content: input,
      strategy: "auto",
      response_format: "minimal",
    });
    const latency = performance.now() - started;
    const second = await autoOptimizeTool.execute({
      content: input,
      strategy: "auto",
      response_format: "minimal",
    });
    const visible = outputText(first);
    const secondVisible = outputText(second);
    const score = scoreProjection(fixture, Buffer.from(visible, "utf8"));
    const metadata = first.structuredContent ?? {};
    const deterministic =
      sha256(Buffer.from(visible)) === sha256(Buffer.from(secondVisible));
    return {
      fixture_id: fixture.id,
      path: "auto_optimize",
      status: deterministic ? "measured" : "invalid",
      input_bytes: sourceBytes.length,
      input_tokens: countTokens(input),
      visible_bytes: Buffer.byteLength(visible),
      visible_tokens: countTokens(visible),
      p0_declared: score.p0.declared,
      p0_recalled: score.p0.recalled,
      p1_declared: score.p1.declared,
      p1_recalled: score.p1.recalled,
      deterministic,
      output_sha256: sha256(Buffer.from(visible)),
      latency_ms_single_observation: round(latency),
      rss_before_bytes: rssBefore,
      rss_after_bytes: process.memoryUsage().rss,
      method: typeof metadata.method === "string" ? metadata.method : null,
      tool_error: first.isError === true,
      failure_mode:
        first.isError === true
          ? "tool_error"
          : deterministic
            ? null
            : "nondeterministic_output",
      error: deterministic
        ? null
        : {
            name: "NondeterministicOutput",
            message: "two consecutive outputs had different SHA-256 digests",
          },
      metadata_loss:
        fixture.source.kind === "process"
          ? [
              "stdout_stderr_event_order",
              "exit_code",
              "signal",
              "timeout",
              "working_directory",
              "truncation_state",
            ]
          : [],
    };
  } catch (error) {
    return invalidCase(
      fixture,
      "auto_optimize",
      sourceBytes.length,
      performance.now() - started,
      error,
    );
  }
}

async function evaluateSmartFixture(fixture, temporaryRoot) {
  const sourceBytes = materializeSource(fixture.source);
  const priorCwd = process.cwd();
  process.chdir(temporaryRoot);
  const started = performance.now();
  try {
    const args = smartArgs(fixture);
    const first = await executeSmartFileRead(args);
    const latency = performance.now() - started;
    const second = await executeSmartFileRead(args);
    const visible = outputText(first);
    const secondVisible = outputText(second);
    const score = scoreProjection(fixture, Buffer.from(visible, "utf8"));
    const deterministic =
      sha256(Buffer.from(visible)) === sha256(Buffer.from(secondVisible));
    return {
      fixture_id: fixture.id,
      path: "smart_file_read",
      status: deterministic ? "measured" : "invalid",
      input_bytes: sourceBytes.length,
      input_tokens: countTokens(sourceBytes.toString("utf8")),
      visible_bytes: Buffer.byteLength(visible),
      visible_tokens: countTokens(visible),
      p0_declared: score.p0.declared,
      p0_recalled: score.p0.recalled,
      p1_declared: score.p1.declared,
      p1_recalled: score.p1.recalled,
      deterministic,
      output_sha256: sha256(Buffer.from(visible)),
      latency_ms_single_observation: round(latency),
      rss_before_bytes: null,
      rss_after_bytes: process.memoryUsage().rss,
      method: args.mode,
      tool_error: first.isError === true,
      failure_mode:
        first.isError === true
          ? "tool_error"
          : deterministic
            ? null
            : "nondeterministic_output",
      error: deterministic
        ? null
        : {
            name: "NondeterministicOutput",
            message: "two consecutive outputs had different SHA-256 digests",
          },
      metadata_loss: [],
    };
  } catch (error) {
    return invalidCase(
      fixture,
      "smart_file_read",
      sourceBytes.length,
      performance.now() - started,
      error,
    );
  } finally {
    process.chdir(priorCwd);
  }
}

async function timeAutoFixture(fixture) {
  const sourceBytes = materializeSource(fixture.source);
  if (sourceBytes.length >= 1024 * 1024) {
    return timeAutoFixtureIsolated(fixture, sourceBytes.length);
  }
  const input = sourceBytes.toString("utf8");
  return timeOperation({
    id: `auto:${fixture.id}`,
    path: "auto_optimize",
    inputBytes: sourceBytes.length,
    operation: async () =>
      outputText(
        await autoOptimizeTool.execute({
          content: input,
          strategy: "auto",
          response_format: "minimal",
        }),
      ),
  });
}

async function timeAutoFixtureIsolated(fixture, inputBytes) {
  const warmupEvidence = [];
  for (let run = 0; run < WARMUP_RUNS; run += 1) {
    const attempt = runIsolatedAutoAttempt(fixture.id);
    warmupEvidence.push({
      run,
      output_sha256: attempt.output_sha256,
      error: attempt.error,
    });
  }

  const durations = [];
  const outputDigests = [];
  const rssSamples = [];
  const errors = [];
  for (let run = 0; run < MEASURED_RUNS; run += 1) {
    const attempt = runIsolatedAutoAttempt(fixture.id);
    durations.push(attempt.duration_ms);
    outputDigests.push(attempt.output_sha256);
    errors.push(attempt.error);
    rssSamples.push(process.memoryUsage().rss);
  }

  const deterministic =
    outputDigests.every((digest) => typeof digest === "string") &&
    new Set(outputDigests).size === 1;
  const valid = errors.every((error) => error === null) && deterministic;
  const sorted = [...durations].sort((left, right) => left - right);
  return {
    id: `auto:${fixture.id}`,
    path: "auto_optimize",
    input_bytes: inputBytes,
    warmup_evidence: warmupEvidence,
    durations_ms: durations,
    output_sha256: outputDigests,
    rss_samples_bytes: rssSamples,
    valid,
    invalid_reason: valid
      ? null
      : errors.some((error) => error !== null)
        ? "execution_error"
        : "nondeterministic_output",
    errors,
    median_ms: valid ? percentile(sorted, 0.5) : null,
    p95_ms: valid ? percentile(sorted, 0.95) : null,
    peak_rss_bytes: rssSamples.length > 0 ? Math.max(...rssSamples) : null,
  };
}

function runIsolatedAutoAttempt(fixtureId) {
  const started = performance.now();
  const child = spawnSync(
    process.execPath,
    [scriptPath, "--probe-auto-output", fixtureId],
    {
      cwd: root,
      encoding: "utf8",
      timeout: LARGE_TIMING_TIMEOUT_MS,
      killSignal: "SIGKILL",
      maxBuffer: 1024 * 1024,
      env: {
        ...process.env,
        DISTILL_COMPRESSED_MARKERS: "",
        [RETRIEVE_ENV_VAR]: "",
      },
    },
  );
  const duration = round(performance.now() - started);
  if (child.status === 0) {
    try {
      return {
        duration_ms: duration,
        output_sha256: JSON.parse(child.stdout).output_sha256,
        error: null,
      };
    } catch (error) {
      return {
        duration_ms: duration,
        output_sha256: null,
        error: safeError(error),
      };
    }
  }
  return {
    duration_ms: duration,
    output_sha256: null,
    error: {
      name: child.error?.code === "ETIMEDOUT" ? "MeasurementTimeout" : "ChildProcessError",
      message:
        child.error?.code === "ETIMEDOUT"
          ? `isolated legacy projection exceeded ${LARGE_TIMING_TIMEOUT_MS} ms`
          : (child.stderr.trim() || child.error?.message || "isolated timing probe failed").slice(
              0,
              500,
            ),
      timeout_ms: LARGE_TIMING_TIMEOUT_MS,
      child_status: child.status,
      child_signal: child.signal,
    },
  };
}

async function timeSmartFixture(fixture, temporaryRoot) {
  const sourceBytes = materializeSource(fixture.source);
  return timeOperation({
    id: `smart:${fixture.id}`,
    path: "smart_file_read",
    inputBytes: sourceBytes.length,
    operation: async () => {
      const priorCwd = process.cwd();
      process.chdir(temporaryRoot);
      try {
        return outputText(await executeSmartFileRead(smartArgs(fixture)));
      } finally {
        process.chdir(priorCwd);
      }
    },
  });
}

async function timeOperation({ id, path, inputBytes, operation }) {
  const warmupEvidence = [];
  for (let run = 0; run < WARMUP_RUNS; run += 1) {
    try {
      warmupEvidence.push({
        run,
        output_sha256: sha256(Buffer.from(await operation())),
        error: null,
      });
    } catch (error) {
      warmupEvidence.push({ run, output_sha256: null, error: safeError(error) });
    }
  }

  const durations = [];
  const outputDigests = [];
  const rssSamples = [];
  const errors = [];
  for (let run = 0; run < MEASURED_RUNS; run += 1) {
    const started = performance.now();
    try {
      const output = await operation();
      durations.push(round(performance.now() - started));
      outputDigests.push(sha256(Buffer.from(output)));
      errors.push(null);
    } catch (error) {
      durations.push(round(performance.now() - started));
      outputDigests.push(null);
      errors.push(safeError(error));
    }
    rssSamples.push(process.memoryUsage().rss);
  }

  const deterministic =
    outputDigests.every((digest) => typeof digest === "string") &&
    new Set(outputDigests).size === 1;
  const valid = errors.every((error) => error === null) && deterministic;
  const sorted = [...durations].sort((left, right) => left - right);
  return {
    id,
    path,
    input_bytes: inputBytes,
    warmup_evidence: warmupEvidence,
    durations_ms: durations,
    output_sha256: outputDigests,
    rss_samples_bytes: rssSamples,
    valid,
    invalid_reason: valid
      ? null
      : errors.some((error) => error !== null)
        ? "execution_error"
        : "nondeterministic_output",
    errors,
    median_ms: valid ? percentile(sorted, 0.5) : null,
    p95_ms: valid ? percentile(sorted, 0.95) : null,
    peak_rss_bytes: rssSamples.length > 0 ? Math.max(...rssSamples) : null,
  };
}

async function measureProcessScopedRecovery(fixture) {
  const sourceBytes = materializeSource(fixture.source);
  const original = sourceBytes.toString("utf8");
  const store = getOriginStore();
  store.clear();
  process.env[RETRIEVE_ENV_VAR] = "1";

  const projected = await autoOptimizeTool.execute({
    content: original,
    strategy: "auto",
    response_format: "minimal",
  });
  const visible = outputText(projected);
  const handle = visible.match(/ctx\.restore\("([a-z0-9]+)"\)/u)?.[1];
  if (!handle) {
    throw new Error("legacy recovery measurement did not emit an origin handle");
  }
  const recovered = store.get(handle);
  const child = spawnSync(process.execPath, [scriptPath, "--probe-origin", handle], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      [RETRIEVE_ENV_VAR]: "1",
    },
  });
  if (child.status !== 0) {
    throw new Error(`fresh-process recovery probe failed: ${child.stderr.trim()}`);
  }

  store.clear();
  let oldestHandle = "";
  for (let index = 0; index < 65; index += 1) {
    const candidate = store.put(`entry-cap-${index}`);
    if (index === 0) {
      oldestHandle = candidate;
    }
  }
  const entryCapObserved = store.size() === 64 && store.get(oldestHandle) === undefined;

  store.clear();
  const oversized = `${"x".repeat(32 * 1024 * 1024)}y`;
  const oversizedHandle = store.put(oversized);
  const byteCapObserved = store.get(oversizedHandle) === undefined && store.size() === 0;
  store.clear();

  const timing = await timeOperation({
    id: "recovery:process-scoped-put-get",
    path: "process_scoped_recovery",
    inputBytes: sourceBytes.length,
    operation: async () => {
      const currentHandle = store.put(original);
      return store.get(currentHandle) ?? "";
    },
  });
  store.clear();
  delete process.env[RETRIEVE_ENV_VAR];

  return {
    result: {
      supported: true,
      scope: "single_process",
      artifact_handle: handle,
      same_process_sha256_equal:
        recovered !== undefined && sha256(Buffer.from(recovered)) === fixture.source_sha256,
      fresh_process_recovery: child.stdout.trim() === "present",
      fresh_process_probe: child.stdout.trim(),
      entry_cap_observed: entryCapObserved,
      entry_cap: 64,
      byte_cap_observed: byteCapObserved,
      byte_cap: 32 * 1024 * 1024,
      failure_mode: "origin_missing_after_process_restart_or_lru_eviction",
      durable: false,
    },
    timing,
  };
}

function aggregateCases(cases) {
  const measured = cases.filter((candidate) => candidate.status === "measured");
  const valid = measured.filter((candidate) => candidate.deterministic);
  const tokenReductions = valid
    .filter((candidate) => candidate.input_tokens > 0)
    .map(
      (candidate) =>
        (candidate.input_tokens - candidate.visible_tokens) / candidate.input_tokens,
    )
    .sort((left, right) => left - right);
  const sum = (field) =>
    valid.reduce((total, candidate) => total + candidate[field], 0);
  const p0Declared = sum("p0_declared");
  const p1Declared = sum("p1_declared");
  return {
    total_cases: cases.length,
    measured_cases: measured.length,
    unsupported_cases: cases.filter((candidate) => candidate.status === "unsupported")
      .length,
    invalid_cases: cases.filter((candidate) => candidate.status === "invalid").length,
    deterministic_cases: valid.length,
    p0_declared: p0Declared,
    p0_recalled: sum("p0_recalled"),
    p0_recall: p0Declared === 0 ? 1 : sum("p0_recalled") / p0Declared,
    p1_declared: p1Declared,
    p1_recalled: sum("p1_recalled"),
    p1_recall: p1Declared === 0 ? 1 : sum("p1_recalled") / p1Declared,
    median_visible_token_reduction:
      tokenReductions.length === 0 ? null : percentile(tokenReductions, 0.5),
  };
}

function unsupportedCase(fixture, path, failureMode, inputBytes) {
  return {
    fixture_id: fixture.id,
    path,
    status: "unsupported",
    input_bytes: inputBytes,
    input_tokens: null,
    visible_bytes: null,
    visible_tokens: null,
    p0_declared: fixture.annotations.p0.length,
    p0_recalled: null,
    p1_declared: fixture.annotations.p1.length,
    p1_recalled: null,
    deterministic: null,
    output_sha256: null,
    latency_ms_single_observation: null,
    rss_before_bytes: process.memoryUsage().rss,
    rss_after_bytes: process.memoryUsage().rss,
    method: null,
    tool_error: null,
    failure_mode: failureMode,
    metadata_loss: [],
  };
}

function invalidCase(fixture, path, inputBytes, latency, error) {
  return {
    fixture_id: fixture.id,
    path,
    status: "invalid",
    input_bytes: inputBytes,
    input_tokens: null,
    visible_bytes: null,
    visible_tokens: null,
    p0_declared: fixture.annotations.p0.length,
    p0_recalled: null,
    p1_declared: fixture.annotations.p1.length,
    p1_recalled: null,
    deterministic: null,
    output_sha256: null,
    latency_ms_single_observation: round(latency),
    rss_before_bytes: null,
    rss_after_bytes: process.memoryUsage().rss,
    method: null,
    tool_error: null,
    failure_mode: "measurement_error",
    error: safeError(error),
    metadata_loss: [],
  };
}

function assertMeasuredCase(candidate) {
  if (!["measured", "unsupported", "invalid"].includes(candidate.status)) {
    throw new Error(`case ${candidate.fixture_id} has an invalid status`);
  }
  const required = [
    "input_bytes",
    "input_tokens",
    "visible_bytes",
    "visible_tokens",
    "p0_declared",
    "p0_recalled",
    "p1_declared",
    "p1_recalled",
    "deterministic",
    "failure_mode",
  ];
  for (const key of required) {
    if (!(key in candidate)) {
      throw new Error(`case ${candidate.fixture_id} is missing ${key}`);
    }
  }
  if (
    candidate.status === "measured" &&
    (!Number.isInteger(candidate.input_tokens) ||
      !Number.isInteger(candidate.visible_tokens) ||
      typeof candidate.deterministic !== "boolean")
  ) {
    throw new Error(`measured case ${candidate.fixture_id} lacks token or determinism data`);
  }
}

function assertTimingCase(candidate, protocol) {
  if (
    candidate.warmup_evidence.length !== protocol.warmup_runs ||
    candidate.durations_ms.length !== protocol.measured_runs ||
    candidate.output_sha256.length !== protocol.measured_runs ||
    candidate.rss_samples_bytes.length !== protocol.measured_runs ||
    candidate.errors.length !== protocol.measured_runs
  ) {
    throw new Error(`timing case ${candidate.id} has incomplete raw evidence`);
  }
  const sorted = [...candidate.durations_ms].sort((left, right) => left - right);
  if (candidate.valid) {
    if (
      candidate.errors.some((error) => error !== null) ||
      new Set(candidate.output_sha256).size !== 1 ||
      candidate.median_ms !== percentile(sorted, 0.5) ||
      candidate.p95_ms !== percentile(sorted, 0.95)
    ) {
      throw new Error(`timing case ${candidate.id} has invalid derived statistics`);
    }
  } else if (candidate.median_ms !== null || candidate.p95_ms !== null) {
    throw new Error(`invalid timing case ${candidate.id} was silently averaged`);
  }
}

function assertCaseCoverage(cases, expectedIds, label) {
  const ids = new Set(cases.map((candidate) => candidate.fixture_id));
  if (
    ids.size !== cases.length ||
    ids.size !== expectedIds.size ||
    [...expectedIds].some((id) => !ids.has(id))
  ) {
    throw new Error(`${label} evidence does not cover its complete fixture set`);
  }
}

async function replayStableAutoCase(fixture, cases) {
  const expected = cases.find((candidate) => candidate.fixture_id === fixture.id);
  const visible = outputText(
    await autoOptimizeTool.execute({
      content: materializeSource(fixture.source).toString("utf8"),
      strategy: "auto",
      response_format: "minimal",
    }),
  );
  if (sha256(Buffer.from(visible)) !== expected.output_sha256) {
    throw new Error("auto_optimize stable replay differs from baseline");
  }
}

async function replayStableSmartCase(fixture, cases, temporaryRoot) {
  const expected = cases.find((candidate) => candidate.fixture_id === fixture.id);
  const priorCwd = process.cwd();
  process.chdir(temporaryRoot);
  try {
    const visible = outputText(await executeSmartFileRead(smartArgs(fixture)));
    if (sha256(Buffer.from(visible)) !== expected.output_sha256) {
      throw new Error("smart_file_read stable replay differs from baseline");
    }
  } finally {
    process.chdir(priorCwd);
  }
}

function smartArgs(fixture) {
  return {
    filePath: fixture.source.relative_path,
    mode: "skeleton",
    cache: false,
    format: "plain",
  };
}

function outputText(result) {
  return result.content
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("");
}

async function materializeFileFixtures(fixtures, directory) {
  for (const fixture of fixtures) {
    if (fixture.source.kind !== "file") {
      continue;
    }
    const path = join(directory, fixture.source.relative_path);
    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
    await writeFile(path, materializeSource(fixture.source), { mode: 0o600 });
  }
}

async function inspectCodexInterception(sourceFiles) {
  const matches = [];
  const patterns = ["PostToolUse", "post_tool_use", "tool_response"];
  for (const path of sourceFiles) {
    const content = await readFile(path, "utf8");
    for (const pattern of patterns) {
      if (content.includes(pattern)) {
        matches.push({
          file: relative(root, path),
          pattern,
        });
      }
    }
  }
  return {
    supported: false,
    production_matches: matches,
    match_classification:
      "Claude Code suggestion-only hooks; they emit reminders and do not replace Codex results",
    failure_mode: "no_supported_codex_result_interception_path",
    visible_tokens: null,
    recovery_behavior: "not_applicable",
  };
}

async function countLegacyLoc(sourceFiles) {
  let productionLines = 0;
  let testLines = 0;
  let productionFiles = 0;
  let testFiles = 0;
  for (const path of sourceFiles) {
    const content = await readFile(path, "utf8");
    const lines = physicalLines(content);
    if (path.endsWith(".test.ts") || path.endsWith("type-tests.ts")) {
      testFiles += 1;
      testLines += lines;
    } else {
      productionFiles += 1;
      productionLines += lines;
    }
  }
  return {
    method:
      "Physical TypeScript lines under packages/mcp-server/src; *.test.ts and type-tests.ts are tests",
    production_files: productionFiles,
    production_lines: productionLines,
    test_files: testFiles,
    test_lines: testLines,
    success_criterion: false,
  };
}

function buildSalvageMatrix(autoSummary, smartSummary, recovery) {
  return [
    {
      behavior: "Deterministic repeatability of supported projections",
      classification: "preserve",
      evidence: `${autoSummary.deterministic_cases}/${autoSummary.measured_cases} auto_optimize and ${smartSummary.deterministic_cases}/${smartSummary.measured_cases} smart_file_read cases produced identical consecutive outputs`,
    },
    {
      behavior: "Content-type routing and extractive reducers",
      classification: "re-evaluate",
      evidence: `Corpus P0 recall ${formatPercent(autoSummary.p0_recall)} and P1 recall ${formatPercent(autoSummary.p1_recall)} do not satisfy the replacement fidelity gate`,
    },
    {
      behavior: "AST-backed structural file projection",
      classification: "re-evaluate",
      evidence: `Corpus P0 recall ${formatPercent(smartSummary.p0_recall)} and P1 recall ${formatPercent(smartSummary.p1_recall)}; structure can omit body facts`,
    },
    {
      behavior: "Current implicit token counter and fallback",
      classification: "re-evaluate",
      evidence:
        "The legacy path maps gpt-4 to cl100k_base but does not expose a contract-bound tokenizer version or fallback status in every result",
    },
    {
      behavior: "Process-scoped origin store",
      classification: "discard",
      evidence: `Same-process recovery ${recovery.same_process_sha256_equal}; fresh-process recovery ${recovery.fresh_process_recovery}`,
    },
    {
      behavior: "Three always-loaded MCP tools",
      classification: "discard",
      evidence:
        "The host owns context aggregation and the new contract is one engine operation, not a fixed tool-count invariant",
    },
    {
      behavior: "QuickJS execution surface",
      classification: "discard",
      evidence:
        "Execution is outside bounded context projection and adds an unrelated security boundary",
    },
    {
      behavior: "Generative summarization",
      classification: "discard",
      evidence:
        "The frozen contract requires deterministic extractive projection and versioned fact preservation",
    },
    {
      behavior: "Compression markers tied to host compaction",
      classification: "re-evaluate",
      evidence:
        "Markers are adapter-specific envelope bytes and must be justified against explicit total-visible budget accounting",
    },
  ];
}

function collectInvalidCases(autoCases, smartCases, timingCases) {
  return [
    ...autoCases
      .filter((candidate) => candidate.status === "invalid")
      .map((candidate) => ({
        id: `corpus:auto:${candidate.fixture_id}`,
        raw_evidence: candidate.error,
      })),
    ...smartCases
      .filter((candidate) => candidate.status === "invalid")
      .map((candidate) => ({
        id: `corpus:smart:${candidate.fixture_id}`,
        raw_evidence: candidate.error,
      })),
    ...timingCases
      .filter((candidate) => !candidate.valid)
      .map((candidate) => ({
        id: `timing:${candidate.id}`,
        raw_evidence: {
          durations_ms: candidate.durations_ms,
          output_sha256: candidate.output_sha256,
          errors: candidate.errors,
          invalid_reason: candidate.invalid_reason,
        },
      })),
  ];
}

function renderReport(evidence) {
  const auto = evidence.paths.auto_optimize;
  const smart = evidence.paths.smart_file_read;
  const recovery = evidence.paths.process_scoped_recovery;
  const codex = evidence.paths.codex_interception;
  const machine = evidence.reference_machine;
  const invalid = evidence.invalid_cases;
  const unsupportedAuto = auto.cases
    .filter((candidate) => candidate.status === "unsupported")
    .reduce((counts, candidate) => {
      counts[candidate.failure_mode] = (counts[candidate.failure_mode] ?? 0) + 1;
      return counts;
    }, {});

  const lines = [
    "# Legacy Distill baseline",
    "",
    `- Evidence schema: \`${evidence.schema_version}\``,
    `- Generated: \`${evidence.generated_at}\``,
    `- Git baseline: \`${evidence.baseline_git_commit}\``,
    `- Corpus: ${evidence.corpus.fixtures} fixtures, manifest SHA-256 \`${evidence.corpus.manifest_sha256}\``,
    "",
    "This report is generated from `evidence.json`. Production and historical PRD files were measured but not edited. LOC is context only and is not a success criterion.",
    "",
    "## Reference machine and protocol",
    "",
    "| Field | Value |",
    "|---|---|",
    `| Platform | ${machine.platform} ${machine.release} (${machine.arch}) |`,
    `| CPU | ${machine.cpu_count} x ${escapeTable(machine.cpu_model)} |`,
    `| Memory | ${machine.total_memory_bytes} bytes |`,
    `| Bun | ${machine.bun_version} |`,
    `| Node compatibility | ${machine.node_version} |`,
    `| Timing | ${evidence.protocol.warmup_runs} warm-ups, then ${evidence.protocol.measured_runs} measured runs per timed case |`,
    `| Clock | ${evidence.protocol.clock}, milliseconds |`,
    `| Peak RSS | ${evidence.protocol.rss_measurement} |`,
    `| Token profile | ${evidence.protocol.token_profile} |`,
    "",
    "## Legacy path summary",
    "",
    "| Path | Coverage | P0 recall | P1 recall | Median visible-token reduction | Determinism | Recovery or failure mode |",
    "|---|---:|---:|---:|---:|---:|---|",
    `| \`auto_optimize\` | ${auto.summary.measured_cases}/${auto.summary.total_cases} measured | ${formatPercent(auto.summary.p0_recall)} | ${formatPercent(auto.summary.p1_recall)} | ${formatPercent(auto.summary.median_visible_token_reduction)} | ${auto.summary.deterministic_cases}/${auto.summary.measured_cases} | ${escapeTable(JSON.stringify(unsupportedAuto))} |`,
    `| \`smart_file_read\` | ${smart.summary.measured_cases}/${smart.summary.total_cases} file fixtures | ${formatPercent(smart.summary.p0_recall)} | ${formatPercent(smart.summary.p1_recall)} | ${formatPercent(smart.summary.median_visible_token_reduction)} | ${smart.summary.deterministic_cases}/${smart.summary.measured_cases} | No durable source artifact |`,
    `| Process-scoped recovery | 1 behavior probe | n/a | n/a | n/a | content-derived handle | same process: ${recovery.same_process_sha256_equal}; fresh process: ${recovery.fresh_process_recovery} |`,
    `| Codex interception | unsupported | n/a | n/a | n/a | n/a | ${codex.failure_mode} |`,
    "",
    "`auto_optimize` receives process observations as flattened text. It does not preserve stdout/stderr event order, exit code, signal, timeout, working directory, or truncation state. Malformed-byte fixtures are unsupported because the legacy interface requires a JavaScript string.",
    "",
    "## Timed cases",
    "",
    "| Case | Input bytes | Median ms | P95 ms | Peak RSS bytes | Valid |",
    "|---|---:|---:|---:|---:|---|",
    ...evidence.timing_cases.map(
      (candidate) =>
        `| \`${candidate.id}\` | ${candidate.input_bytes} | ${formatNumber(candidate.median_ms)} | ${formatNumber(candidate.p95_ms)} | ${candidate.peak_rss_bytes ?? "n/a"} | ${candidate.valid} |`,
    ),
    "",
    "Every valid row retains its two warm-up output digests, ten raw durations, ten output digests, ten RSS samples, and per-run errors in `evidence.json`. Invalid rows are not averaged.",
    "",
    "## Recovery behavior",
    "",
    `The optional origin store recovered SHA-256-identical bytes inside one process: ${recovery.same_process_sha256_equal}. A fresh process reported \`${recovery.fresh_process_probe}\`, so restart recovery is ${recovery.fresh_process_recovery}. The measured LRU entry cap was ${recovery.entry_cap}; the measured byte cap was ${recovery.byte_cap} bytes. Its failure mode is \`${recovery.failure_mode}\`.`,
    "",
    "## LOC context",
    "",
    `Method: ${evidence.loc.method}.`,
    "",
    "| Class | Files | Physical lines |",
    "|---|---:|---:|",
    `| Production TypeScript | ${evidence.loc.production_files} | ${evidence.loc.production_lines} |`,
    `| Test TypeScript | ${evidence.loc.test_files} | ${evidence.loc.test_lines} |`,
    "",
    "These counts describe migration size only. They are not a projection-quality target.",
    "",
    "## Salvage matrix",
    "",
    "| Legacy behavior | Classification | Evidence |",
    "|---|---|---|",
    ...evidence.salvage_matrix.map(
      (entry) =>
        `| ${escapeTable(entry.behavior)} | \`${entry.classification}\` | ${escapeTable(entry.evidence)} |`,
    ),
    "",
    "## Invalid measurements",
    "",
    invalid.length === 0
      ? "No crashed, nondeterministic, or incomplete timed measurement was observed."
      : `${invalid.length} invalid measurement(s) remain in raw evidence and are excluded from aggregates: ${invalid.map((entry) => `\`${entry.id}\``).join(", ")}.`,
    "",
  ];
  return `${lines.join("\n")}\n`;
}

async function digestFiles(paths) {
  const hash = createHash("sha256");
  for (const path of paths) {
    hash.update(relative(root, path));
    hash.update("\0");
    hash.update(await readFile(path));
    hash.update("\0");
  }
  return hash.digest("hex");
}

async function listFiles(directory, predicate) {
  const paths = [];
  const entries = await readdir(directory, { withFileTypes: true });
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      paths.push(...(await listFiles(path, predicate)));
    } else if (entry.isFile() && predicate(path)) {
      paths.push(path);
    }
  }
  return paths.sort();
}

function referenceMachine() {
  const cpuList = cpus();
  return {
    platform: platform(),
    arch: arch(),
    release: release(),
    cpu_model: cpuList[0]?.model ?? "unknown",
    cpu_count: cpuList.length,
    total_memory_bytes: totalmem(),
    bun_version: Bun.version,
    node_version: process.version,
  };
}

function assertLegacySourceIsClean() {
  const status = git([
    "status",
    "--porcelain=v1",
    "--untracked-files=all",
    "--",
    "packages/mcp-server/src",
  ]).trim();
  if (status.length > 0) {
    throw new Error("legacy production source has tracked or untracked worktree changes");
  }
}

function git(args) {
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.trim()}`);
  }
  return result.stdout;
}

function physicalLines(content) {
  if (content.length === 0) {
    return 0;
  }
  const separators = content.match(/\r\n|\r|\n/gu)?.length ?? 0;
  return separators + (content.endsWith("\n") || content.endsWith("\r") ? 0 : 1);
}

function safeError(error) {
  if (error instanceof Error) {
    return {
      name: error.name,
      message: error.message.slice(0, 500),
    };
  }
  return {
    name: "UnknownError",
    message: String(error).slice(0, 500),
  };
}

function round(value) {
  return Math.round(value * 1_000_000) / 1_000_000;
}

function formatPercent(value) {
  return value === null || value === undefined
    ? "n/a"
    : `${(value * 100).toFixed(1)}%`;
}

function formatNumber(value) {
  return value === null || value === undefined ? "invalid" : value.toFixed(3);
}

function escapeTable(value) {
  return String(value).replaceAll("|", "\\|").replaceAll("\n", " ");
}

function restoreEnvironment(name, value) {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}
