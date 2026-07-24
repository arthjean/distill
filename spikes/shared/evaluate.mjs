import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  mkdir,
  mkdtemp,
  open,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { cpus, hostname, platform, release } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { materializeSource } from "../../evaluation/corpus/lib.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "../..");
const EVIDENCE_DIR = join(ROOT, "spikes/evidence");
const MANIFEST = join(ROOT, "evaluation/corpus/manifest.jsonl");
const PROFILES = join(ROOT, "evaluation/corpus/budget-profiles.json");
const SUBSET = join(HERE, "corpus-subset.json");
const PROCESS_FIXTURE = join(HERE, "process-fixture.mjs");
const PROTOCOL = "distill.spike/v1";
const CLOCK_TICKS =
  platform() === "linux"
    ? Number(runProgram("getconf", ["CLK_TCK"], "").stdout.trim())
    : null;

const [mode, candidate, binaryArgument, ...options] = process.argv.slice(2);
if (!["conformance", "benchmark", "fuzz"].includes(mode)) {
  throw new Error("mode must be conformance, benchmark, or fuzz");
}
if (!["zig", "rust"].includes(candidate) || !binaryArgument) {
  throw new Error("candidate must be zig or rust and binary path is required");
}
const binary = resolve(binaryArgument);
await stat(binary);
await mkdir(EVIDENCE_DIR, { recursive: true, mode: 0o700 });
await chmod(EVIDENCE_DIR, 0o700);

const machine = {
  hostname: hostname(),
  platform: platform(),
  release: release(),
  architecture: process.arch,
  logical_cpus: cpus().length,
  node: process.version,
  sqlite: runProgram("sqlite3", ["--version"], "").stdout.trim(),
};

if (mode === "conformance") {
  await writeEvidence("conformance", await conformance());
} else if (mode === "benchmark") {
  await writeEvidence("benchmark", await benchmark());
} else {
  const cpuSeconds = numberOption("--cpu-seconds", 3600);
  const workers = numberOption(
    "--workers",
    Math.max(1, Math.min(4, Math.floor(cpus().length / 2))),
  );
  await writeEvidence("fuzz", await fuzz(cpuSeconds, workers));
}

async function conformance() {
  const startedAt = new Date().toISOString();
  const root = await mkdtemp(
    join(EVIDENCE_DIR, `.tmp-${candidate}-conformance-`),
  );
  await chmod(root, 0o700);
  const checks = [];
  try {
    const health = invoke(
      request("health", join(root, "health.sqlite"), { request_id: "health" }),
    );
    check(health.ok, "health response succeeds");
    check(
      health.outcome.candidate === candidate,
      "candidate identifies itself",
    );
    check(health.outcome.protocol === PROTOCOL, "candidate protocol matches");
    checks.push("health");
    const runtimeDependencies = runProgram("ldd", [binary], "").stdout.trim();
    check(
      !/(libcurl|libssl|libcrypto|libssh|libnghttp)/i.test(runtimeDependencies),
      "release binary has no network runtime dependency",
    );
    checks.push("no-runtime-network-dependency");

    const invalid = runProgram(binary, [], "{not-json}\n");
    const invalidResponse = parseSingle(invalid.stdout);
    check(
      invalidResponse.failure?.code === "invalid_request",
      "invalid JSON is typed",
    );
    checks.push("invalid-json");

    const token = invoke(
      projectRequest({
        storePath: join(root, "token.sqlite"),
        requestId: "token",
        bytes: Buffer.from("token profile"),
        budget: {
          unit: "tokens",
          total_visible_limit: 32,
          reserved_envelope: 4,
          token_profile: "cl100k_base@js-tiktoken-1.0.15",
        },
      }),
    );
    check(
      token.failure?.code === "token_profile_unsupported",
      "unsupported tokenizer is explicit",
    );
    checks.push("token-profile");

    const fixtures = await corpusInputs();
    for (const fixture of fixtures) {
      const storePath = join(root, `${fixture.id}.sqlite`);
      const response = invoke(
        projectRequest({
          storePath,
          requestId: fixture.id,
          bytes: fixture.bytes,
          budget: fixture.budget,
          annotations: fixture.encoded
            ? { p0: [], p1: [] }
            : fixture.annotations,
        }),
      );
      check(
        response.ok,
        `${fixture.id} projects: ${JSON.stringify(response.failure)}`,
      );
      const visible = Buffer.from(response.outcome.visible_base64, "base64");
      check(
        visible.length <=
          fixture.budget.total_visible_limit - fixture.budget.reserved_envelope,
        `${fixture.id} respects payload budget`,
      );
      check(
        response.outcome.receipt.source_sha256 === fixture.source_sha256,
        `${fixture.id} receipt binds source`,
      );
      if (!fixture.encoded) {
        for (const fact of fixture.annotations.p0) {
          check(
            visible.includes(Buffer.from(fact.needle_base64, "base64")),
            `${fixture.id} preserves P0 ${fact.id}`,
          );
        }
        for (const fact of fixture.annotations.p1) {
          check(
            visible.includes(Buffer.from(fact.needle_base64, "base64")),
            `${fixture.id} preserves P1 ${fact.id}`,
          );
        }
      }
      const recovered = invoke(
        request("recover", storePath, {
          request_id: `${fixture.id}-recover`,
          artifact_id: response.outcome.artifact.id,
        }),
      );
      check(recovered.ok, `${fixture.id} recovers`);
      check(
        Buffer.from(recovered.outcome.bytes_base64, "base64").equals(
          fixture.bytes,
        ),
        `${fixture.id} recovery is byte-identical`,
      );

      const second = invoke(
        projectRequest({
          storePath: join(root, `${fixture.id}-determinism.sqlite`),
          requestId: fixture.id,
          bytes: fixture.bytes,
          budget: fixture.budget,
          annotations: fixture.encoded
            ? { p0: [], p1: [] }
            : fixture.annotations,
        }),
      );
      check(
        second.outcome.visible_base64 === response.outcome.visible_base64,
        `${fixture.id} projection is deterministic`,
      );
      check(
        stableReceipt(second.outcome.receipt) ===
          stableReceipt(response.outcome.receipt),
        `${fixture.id} deterministic receipt fields match`,
      );
    }
    checks.push(`corpus-subset:${fixtures.length}`);

    const processResponse = invoke(
      projectRequest({
        operation: "run",
        storePath: join(root, "process.sqlite"),
        requestId: "process-argv",
        source: {
          kind: "process",
          executable: process.execPath,
          argv: [
            PROCESS_FIXTURE,
            "argv",
            "--literal",
            "value with spaces",
            "$(not-shell)",
          ],
          cwd: ROOT,
          timeout_ms: 2_000,
          output_limit: 4_096,
        },
        bytes: null,
        budget: {
          unit: "bytes",
          total_visible_limit: 2048,
          reserved_envelope: 128,
        },
        annotations: {
          p0: [
            {
              id: "argv",
              needle_base64: Buffer.from(
                'argv:["--literal","value with spaces","$(not-shell)"]',
              ).toString("base64"),
            },
          ],
          p1: [
            {
              id: "stderr",
              needle_base64: Buffer.from("synthetic-stderr").toString("base64"),
            },
          ],
        },
      }),
    );
    check(processResponse.ok, "argv-only process fixture succeeds");
    const processVisible = Buffer.from(
      processResponse.outcome.visible_base64,
      "base64",
    ).toString();
    check(
      processVisible.includes("$(not-shell)"),
      "argv is not shell-expanded",
    );
    check(
      processResponse.outcome.receipt.acquisition.events.length === 2,
      "process stream events are ordered",
    );
    checks.push("process-argv");

    for (const [fixtureMode, timeout, label] of [
      ["timeout", 25, "timeout"],
      ["chatter", 50, "chatter"],
      ["closed-streams", 50, "closed-streams"],
      ["signal", 1_000, "signal"],
      ["descendant", 100, "descendant"],
    ]) {
      const processStartedAt = performance.now();
      const descendantMarker = join(root, "descendant-survived");
      const response = invoke(
        projectRequest({
          operation: "run",
          storePath: join(root, `process-${label}.sqlite`),
          requestId: `process-${label}`,
          source: {
            kind: "process",
            executable: process.execPath,
            argv: [
              PROCESS_FIXTURE,
              fixtureMode,
              ...(fixtureMode === "descendant" ? [descendantMarker] : []),
            ],
            cwd: ROOT,
            timeout_ms: timeout,
            output_limit: 4_096,
          },
          bytes: null,
          budget: {
            unit: "bytes",
            total_visible_limit: 2048,
            reserved_envelope: 128,
          },
          annotations: { p0: [], p1: [] },
        }),
      );
      check(
        response.failure?.code === "acquisition_failed",
        `${label} process returns typed failure`,
      );
      if (["chatter", "closed-streams", "descendant"].includes(fixtureMode)) {
        check(
          performance.now() - processStartedAt < 500,
          `${label} process cannot extend the absolute deadline`,
        );
      }
      if (fixtureMode === "descendant") {
        await delay(400);
        const markerExists = await stat(descendantMarker).then(
          () => true,
          () => false,
        );
        check(!markerExists, "descendant process group is terminated");
      }
    }
    checks.push("process-failures");

    const storeFull = invoke(
      projectRequest({
        storePath: join(root, "full.sqlite"),
        requestId: "store-full",
        bytes: Buffer.from("12345"),
        maxStoreBytes: 4,
      }),
    );
    check(storeFull.failure?.code === "store_full", "store cap is typed");
    check(!storeFull.failure.artifact, "store-full has no artifact reference");
    checks.push("store-full");

    const badParent = join(root, "not-a-directory");
    await writeFile(badParent, "file");
    const commitFailure = invoke(
      projectRequest({
        storePath: join(badParent, "store.sqlite"),
        requestId: "commit-failure",
        bytes: Buffer.from("must not escape"),
      }),
    );
    check(
      ["commit_failed", "permission_denied"].includes(
        commitFailure.failure?.code,
      ),
      "failed store initialization is typed",
    );
    check(
      !commitFailure.failure.artifact,
      "failed commit emits no artifact reference",
    );
    checks.push("commit-ordering");

    for (const faultPoint of [
      "before_insert",
      "before_commit",
      "after_commit",
      "before_readback",
    ]) {
      const faultStore = join(root, `fault-${faultPoint}.sqlite`);
      const source = Buffer.from(`atomic-${faultPoint}`);
      const faultRequest = projectRequest({
        storePath: faultStore,
        requestId: `fault-${faultPoint}`,
        bytes: source,
        faultPoint,
      });
      const killed = spawnSync(binary, [], {
        input: `${JSON.stringify(faultRequest)}\n`,
        encoding: "utf8",
        maxBuffer: 4 * 1024 * 1024,
        env: { ...process.env, DISTILL_SPIKE_ENABLE_FAULTS: "1" },
      });
      check(killed.status === 86, `${faultPoint} terminates at the kill point`);
      check(
        killed.stdout.trim() === "",
        `${faultPoint} emits no artifact reference`,
      );
      const integrity = runProgram(
        "sqlite3",
        [faultStore, "PRAGMA integrity_check; SELECT count(*) FROM artifacts;"],
        "",
      )
        .stdout.trim()
        .split("\n");
      check(integrity[0] === "ok", `${faultPoint} leaves SQLite consistent`);
      const committed = ["after_commit", "before_readback"].includes(
        faultPoint,
      );
      check(
        Number(integrity[1]) === (committed ? 1 : 0),
        `${faultPoint} leaves prior-or-complete state`,
      );
      if (committed) {
        const artifactId = runProgram(
          "sqlite3",
          [faultStore, "SELECT id FROM artifacts LIMIT 1;"],
          "",
        ).stdout.trim();
        const recovered = invoke(
          request("recover", faultStore, {
            request_id: `fault-${faultPoint}-recover`,
            artifact_id: artifactId,
          }),
        );
        check(recovered.ok, `${faultPoint} committed source recovers`);
        check(
          Buffer.from(recovered.outcome.bytes_base64, "base64").equals(source),
          `${faultPoint} committed source is complete`,
        );
      }
    }
    checks.push("crash-atomicity");

    const corruptStore = join(root, "corrupt.sqlite");
    const captured = invoke(
      projectRequest({
        storePath: corruptStore,
        requestId: "corrupt-capture",
        bytes: Buffer.from("integrity source"),
      }),
    );
    check(captured.ok, "corruption fixture captures");
    runProgram(
      "sqlite3",
      [
        corruptStore,
        `UPDATE artifacts SET source = x'00' WHERE id = '${captured.outcome.artifact.id}'`,
      ],
      "",
    );
    const corrupt = invoke(
      request("recover", corruptStore, {
        request_id: "corrupt-recover",
        artifact_id: captured.outcome.artifact.id,
      }),
    );
    check(
      corrupt.failure?.code === "artifact_corrupt",
      "corrupt source is distinguished",
    );
    const pageStore = join(root, "page-corrupt.sqlite");
    const pageCaptured = invoke(
      projectRequest({
        storePath: pageStore,
        requestId: "page-corrupt-capture",
        bytes: Buffer.from("page integrity source"),
      }),
    );
    check(pageCaptured.ok, "database corruption fixture captures");
    const pageFile = await open(pageStore, "r+");
    await pageFile.write(Buffer.from("NOT-A-SQLITE-DB!"), 0, 16, 0);
    await pageFile.close();
    const pageCorrupt = invoke(
      request("recover", pageStore, {
        request_id: "page-corrupt-recover",
        artifact_id: pageCaptured.outcome.artifact.id,
      }),
    );
    check(
      pageCorrupt.failure?.code === "artifact_corrupt",
      "damaged database pages are distinguished",
    );
    checks.push("artifact-corrupt");

    const busyStore = join(root, "busy.sqlite");
    const busySeed = invoke(
      projectRequest({
        storePath: busyStore,
        requestId: "busy-seed",
        bytes: Buffer.from("seed"),
      }),
    );
    check(busySeed.ok, "busy fixture initializes");
    const locker = spawn("sqlite3", [busyStore], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    let lockerOutput = "";
    locker.stdout.setEncoding("utf8");
    locker.stdout.on("data", (chunk) => {
      lockerOutput += chunk;
    });
    locker.stdin.write("BEGIN EXCLUSIVE;\n.print locked\n");
    await waitUntil(() => lockerOutput.includes("locked"), 2_000);
    const busy = invoke(
      projectRequest({
        storePath: busyStore,
        requestId: "busy-write",
        bytes: Buffer.from("blocked"),
        busyTimeoutMs: 25,
      }),
    );
    locker.stdin.end("ROLLBACK;\n");
    await childExit(locker);
    check(busy.failure?.code === "store_busy", "busy timeout is typed");
    checks.push("store-busy");

    const databaseMode =
      (await stat(join(root, "build-output-02.sqlite"))).mode & 0o777;
    const rootMode = (await stat(root)).mode & 0o777;
    check(databaseMode === 0o600, "database mode is 0600");
    check(rootMode === 0o700, "store root mode is 0700");
    checks.push("posix-permissions");

    return {
      schema_version: "distill.spike-conformance/v1",
      candidate,
      binary,
      started_at: startedAt,
      completed_at: new Date().toISOString(),
      machine,
      protocol_schema_sha256: await fileSha256(
        join(HERE, "protocol.schema.json"),
      ),
      corpus_subset_sha256: await fileSha256(SUBSET),
      runtime_dependencies: runtimeDependencies.split("\n"),
      checks,
      checks_passed: checks.length,
      checks_failed: 0,
      pass: true,
    };
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function benchmark() {
  const startedAt = new Date().toISOString();
  const root = await mkdtemp(
    join(EVIDENCE_DIR, `.tmp-${candidate}-benchmark-`),
  );
  await chmod(root, 0o700);
  try {
    const healthRequest = `${JSON.stringify(
      request("health", join(root, "health.sqlite"), {
        request_id: "cold-start",
      }),
    )}\n`;
    for (let index = 0; index < 2; index += 1) {
      runProgram(binary, [], healthRequest);
    }
    const coldStartMs = [];
    for (let index = 0; index < 30; index += 1) {
      const start = performance.now();
      runProgram(binary, [], healthRequest);
      coldStartMs.push(performance.now() - start);
    }

    const tenMiB = (await corpusInputs()).find(
      (fixture) => fixture.id === "boundary-one-mib",
    );
    check(tenMiB, "1 MiB benchmark fixture is available");
    const projectionMs = [];
    for (let index = 0; index < 12; index += 1) {
      const requestValue = projectRequest({
        storePath: join(root, `projection-${index}.sqlite`),
        requestId: "projection-benchmark",
        bytes: tenMiB.bytes,
        budget: tenMiB.budget,
        annotations: tenMiB.annotations,
      });
      const start = performance.now();
      const response = invoke(requestValue);
      check(response.ok, "projection benchmark succeeds");
      projectionMs.push(performance.now() - start);
    }
    const timedProjectionMs = projectionMs.slice(2);

    const tenMiBFixture = await fixtureById("boundary-ten-mib");
    const rssRequest = projectRequest({
      storePath: join(root, "rss.sqlite"),
      requestId: "rss-10mib",
      bytes: tenMiBFixture.bytes,
      budget: tenMiBFixture.budget,
      annotations: tenMiBFixture.annotations,
    });
    const timeResult = spawnSync(
      "/usr/bin/time",
      ["-f", "__MAX_RSS_KIB__=%M", binary],
      {
        input: `${JSON.stringify(rssRequest)}\n`,
        encoding: "utf8",
        maxBuffer: 64 * 1024 * 1024,
      },
    );
    check(timeResult.status === 0, "RSS benchmark process succeeds");
    const rssMatch = timeResult.stderr.match(/__MAX_RSS_KIB__=(\d+)/);
    check(rssMatch, "RSS benchmark reports peak memory");
    const peakRssMib = Number(rssMatch[1]) / 1024;
    const coldP95 = percentile(coldStartMs, 0.95);

    return {
      schema_version: "distill.spike-benchmark/v1",
      candidate,
      binary,
      started_at: startedAt,
      completed_at: new Date().toISOString(),
      machine,
      optimization: "release",
      warmup_runs: 2,
      measured_runs: {
        cold_start: 30,
        one_mib_projection: 10,
      },
      cold_start_ms: {
        raw: coldStartMs,
        median: percentile(coldStartMs, 0.5),
        p95: coldP95,
      },
      one_mib_projection_ms: {
        raw: timedProjectionMs,
        median: percentile(timedProjectionMs, 0.5),
        p95: percentile(timedProjectionMs, 0.95),
      },
      ten_mib_peak_rss_mib: peakRssMib,
      knockout: {
        cold_start_p95_at_most_50_ms: coldP95 <= 50,
        ten_mib_peak_rss_at_most_128_mib: peakRssMib <= 128,
      },
      pass: coldP95 <= 50 && peakRssMib <= 128,
    };
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

async function fuzz(targetCpuSeconds, workerCount) {
  if (platform() !== "linux") {
    throw new Error("CPU-accounted fuzz runner currently requires Linux /proc");
  }
  const startedAt = new Date().toISOString();
  const wallStart = performance.now();
  const root = await mkdtemp(join(EVIDENCE_DIR, `.tmp-${candidate}-fuzz-`));
  await chmod(root, 0o700);
  const workers = [];
  let stop = false;
  let crashes = 0;

  try {
    for (let index = 0; index < workerCount; index += 1) {
      const child = spawn(binary, [], {
        stdio: ["pipe", "pipe", "pipe"],
      });
      const state = {
        index,
        child,
        inputLines: 0,
        outputLines: 0,
        stderr: "",
        exited: false,
      };
      child.stdout.on("data", (chunk) => {
        state.outputLines += countByte(chunk, 10);
      });
      child.stderr.setEncoding("utf8");
      child.stderr.on("data", (chunk) => {
        state.stderr = `${state.stderr}${chunk}`.slice(-4096);
      });
      child.on("exit", (code) => {
        state.exited = true;
        if (!stop || code !== 0) crashes += 1;
      });
      workers.push(state);
    }

    const pumps = workers.map((state) =>
      pumpFuzzWorker(state, root, () => stop),
    );
    let cpuSeconds = 0;
    while (cpuSeconds < targetCpuSeconds) {
      await delay(1_000);
      cpuSeconds = 0;
      for (const state of workers) {
        if (state.exited) {
          throw new Error(`fuzz worker ${state.index} exited early`);
        }
        state.lastCpuSeconds = await childCpuSeconds(state.child.pid);
        cpuSeconds += state.lastCpuSeconds;
      }
    }
    stop = true;
    for (const state of workers) state.child.stdin.end();
    await Promise.all(pumps);
    await Promise.all(workers.map((state) => childExit(state.child)));

    const finalCpuSeconds = workers
      .map((state) => state.finalCpuSeconds ?? state.lastCpuSeconds ?? 0)
      .reduce((sum, value) => sum + value, 0);
    const inputLines = workers.reduce(
      (sum, state) => sum + state.inputLines,
      0,
    );
    const outputLines = workers.reduce(
      (sum, state) => sum + state.outputLines,
      0,
    );
    check(crashes === 0, "fuzz workers do not crash");
    check(outputLines === inputLines, "every fuzz request has one response");

    return {
      schema_version: "distill.spike-fuzz/v1",
      candidate,
      binary,
      started_at: startedAt,
      completed_at: new Date().toISOString(),
      machine,
      seed: "distill-ep002-v1",
      workers: workerCount,
      target_cpu_seconds: targetCpuSeconds,
      measured_cpu_seconds: finalCpuSeconds,
      wall_seconds: (performance.now() - wallStart) / 1000,
      cases: inputLines,
      responses: outputLines,
      crashes,
      pass:
        finalCpuSeconds >= targetCpuSeconds &&
        crashes === 0 &&
        outputLines === inputLines,
    };
  } finally {
    stop = true;
    for (const state of workers) {
      if (!state.exited) state.child.kill("SIGTERM");
    }
    await rm(root, { recursive: true, force: true });
  }
}

async function pumpFuzzWorker(state, root, shouldStop) {
  const batch = fuzzBatch(state.index, root);
  while (!shouldStop() && !state.exited) {
    state.inputLines += batch.lineCount;
    if (!state.child.stdin.write(batch.contents)) {
      await new Promise((resolveDrain) =>
        state.child.stdin.once("drain", resolveDrain),
      );
    }
  }
  if (state.child.pid && !state.exited) {
    state.finalCpuSeconds = await childCpuSeconds(state.child.pid).catch(
      () => 0,
    );
  }
}

function fuzzBatch(worker, root) {
  const storePath = join(root, `worker-${worker}.sqlite`);
  const largeBytes = deterministicBytes(worker, 0, 10 * 1024 * 1024);
  const validProject = projectRequest({
    storePath,
    requestId: `project-${worker}`,
    bytes: largeBytes,
    budget: {
      unit: "bytes",
      total_visible_limit: 256,
      reserved_envelope: 0,
    },
    annotations: { p0: [], p1: [] },
    maxStoreBytes: 1,
  });
  const invalidBase64 = structuredClone(validProject);
  invalidBase64.request_id = `invalid-base64-${worker}`;
  invalidBase64.source.bytes_base64 = `${invalidBase64.source.bytes_base64.slice(0, -1)}!`;
  const largeRequestId = request("health", storePath, {
    request_id: "x".repeat(2 * 1024 * 1024),
  });
  const lines = [
    JSON.stringify(validProject),
    JSON.stringify(invalidBase64),
    JSON.stringify(largeRequestId),
    `{"schema_version":"${"x".repeat(2 * 1024 * 1024)}`,
  ];
  return {
    contents: `${lines.join("\n")}\n`,
    lineCount: lines.length,
  };
}

function projectRequest({
  operation = "project",
  storePath,
  requestId,
  bytes = Buffer.from("source bytes"),
  source,
  budget = {
    unit: "bytes",
    total_visible_limit: 2048,
    reserved_envelope: 128,
  },
  annotations = { p0: [], p1: [] },
  maxStoreBytes,
  busyTimeoutMs,
  faultPoint,
}) {
  return request(operation, storePath, {
    request_id: requestId,
    source: source ?? {
      kind: "inline",
      bytes_base64: bytes.toString("base64"),
    },
    budget,
    retention: { ttl_seconds: 3600 },
    preservation: {
      profile: "fixture-facts/v1",
      p0: annotations.p0,
      p1: annotations.p1,
    },
    ...(maxStoreBytes ? { max_store_bytes: maxStoreBytes } : {}),
    ...(busyTimeoutMs ? { busy_timeout_ms: busyTimeoutMs } : {}),
    ...(faultPoint ? { fault_point: faultPoint } : {}),
  });
}

function request(operation, storePath, fields = {}) {
  return {
    schema_version: PROTOCOL,
    request_id: fields.request_id ?? `${operation}-request`,
    operation,
    store_path: storePath,
    ...fields,
  };
}

function invoke(value) {
  const result = runProgram(binary, [], `${JSON.stringify(value)}\n`);
  return parseSingle(result.stdout);
}

function runProgram(command, args, input) {
  const result = spawnSync(command, args, {
    input,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `${command} exited ${result.status}: ${String(result.stderr).slice(-2000)}`,
    );
  }
  return result;
}

function parseSingle(stdout) {
  const lines = stdout.trim().split("\n").filter(Boolean);
  check(lines.length === 1, "candidate emits exactly one JSON line");
  return JSON.parse(lines[0]);
}

async function corpusInputs() {
  const subset = JSON.parse(await readFile(SUBSET, "utf8"));
  const ids = new Set(subset.fixture_ids);
  const lines = (await readFile(MANIFEST, "utf8")).trim().split("\n");
  const profilesDocument = JSON.parse(await readFile(PROFILES, "utf8"));
  const profiles = new Map(
    profilesDocument.profiles.map((profile) => [profile.id, profile]),
  );
  return lines
    .map((line) => JSON.parse(line))
    .filter((fixture) => ids.has(fixture.id))
    .map((fixture) => ({
      ...fixture,
      bytes: materializeSource(fixture.source),
      budget: protocolBudget(profiles.get(fixture.budget_profile)),
      encoded: fixture.expected.utf8_valid === false,
    }));
}

async function fixtureById(id) {
  const lines = (await readFile(MANIFEST, "utf8")).trim().split("\n");
  const fixture = lines
    .map((line) => JSON.parse(line))
    .find((item) => item.id === id);
  check(fixture, `${id} fixture exists`);
  const profilesDocument = JSON.parse(await readFile(PROFILES, "utf8"));
  const profile = profilesDocument.profiles.find(
    (candidateProfile) => candidateProfile.id === fixture.budget_profile,
  );
  return {
    ...fixture,
    bytes: materializeSource(fixture.source),
    budget: protocolBudget(profile),
  };
}

function protocolBudget(profile) {
  return {
    unit: profile.unit,
    total_visible_limit: profile.total_visible_limit,
    reserved_envelope: profile.reserved_envelope,
    ...(profile.token_profile ? { token_profile: profile.token_profile } : {}),
  };
}

function stableReceipt(receipt) {
  return JSON.stringify({
    source_sha256: receipt.source_sha256,
    projection_version: receipt.projection_version,
    policy_version: receipt.policy_version,
    original_count: receipt.original_count,
    visible_count: receipt.visible_count,
    count_unit: receipt.count_unit,
    fidelity: receipt.fidelity,
    retained_spans: receipt.retained_spans,
    omitted_spans: receipt.omitted_spans,
    preservation: receipt.preservation,
    acquisition: receipt.acquisition,
  });
}

function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
}

async function childCpuSeconds(pid) {
  const contents = await readFile(`/proc/${pid}/stat`, "utf8");
  const fields = contents.slice(contents.lastIndexOf(")") + 2).split(" ");
  return (Number(fields[11]) + Number(fields[12])) / CLOCK_TICKS;
}

function deterministicBytes(worker, sequence, length) {
  const output = Buffer.allocUnsafe(length);
  let offset = 0;
  let counter = 0;
  while (offset < length) {
    const chunk = createHash("sha256")
      .update(`distill-ep002-v1:${worker}:${sequence}:${counter}`)
      .digest();
    chunk.copy(output, offset, 0, Math.min(chunk.length, length - offset));
    offset += chunk.length;
    counter += 1;
  }
  return output;
}

function countByte(buffer, byte) {
  let count = 0;
  for (const value of buffer) if (value === byte) count += 1;
  return count;
}

function numberOption(name, fallback) {
  const index = options.indexOf(name);
  if (index === -1) return fallback;
  const value = Number(options[index + 1]);
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error(`${name} must be a positive number`);
  }
  return value;
}

async function writeEvidence(kind, evidence) {
  const path = join(EVIDENCE_DIR, `${candidate}-${kind}.json`);
  await writeFile(path, `${JSON.stringify(evidence, null, 2)}\n`, {
    mode: 0o600,
  });
  await chmod(path, 0o600);
  process.stdout.write(
    `${candidate} ${kind}: ${evidence.pass ? "PASS" : "FAIL"} (${path})\n`,
  );
}

async function fileSha256(path) {
  return createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
}

async function waitUntil(predicate, timeoutMs) {
  const deadline = performance.now() + timeoutMs;
  while (!predicate()) {
    if (performance.now() > deadline) throw new Error("wait timed out");
    await delay(10);
  }
}

function childExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve({ code: child.exitCode, signal: child.signalCode });
  }
  return new Promise((resolveExit) =>
    child.once("exit", (code, signal) => resolveExit({ code, signal })),
  );
}

function delay(milliseconds) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));
}

function check(condition, message) {
  if (!condition) throw new Error(message);
}
