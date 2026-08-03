import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { generateCorpus } from "./generate.mjs";
import {
  loadProfiles,
  readManifest,
  scanSecrets,
  scoreProjection,
  serializeManifest,
  validateCorpus,
  validateFixture,
} from "./lib.mjs";
import {
  parseRealManifest,
  sha256 as realSha256,
  validateRealCorpus,
} from "./real/lib.mjs";

const directory = dirname(fileURLToPath(import.meta.url));
const manifestPath = join(directory, "manifest.jsonl");
const profilesPath = join(directory, "budget-profiles.json");
const schemaPath = join(directory, "schema.json");
const realDirectory = join(directory, "real");
const realManifestPath = join(realDirectory, "manifest.jsonl");
const realCapturePath = join(realDirectory, "capture.mjs");

const mode = process.argv[2];
if (mode !== "--write" && mode !== "--verify") {
  throw new Error("usage: bun evaluation/corpus/check.mjs --write|--verify");
}

const profiles = await loadProfiles(profilesPath);
const generated = generateCorpus();
const summary = validateCorpus(generated, profiles);
const findings = scanSecrets(generated);
if (findings.length > 0) {
  throw new Error(`secret scan failed: ${JSON.stringify(findings)}`);
}

runRejectionTests(generated[0], profiles);
runOracleTest(generated[0]);

const serialized = serializeManifest(generated);
if (mode === "--write") {
  await writeFile(manifestPath, serialized, { encoding: "utf8", mode: 0o600 });
} else {
  const committed = await readManifest(manifestPath);
  validateCorpus(committed, profiles);
  const committedText = await readFile(manifestPath, "utf8");
  if (committedText !== serialized) {
    throw new Error(
      "manifest differs from deterministic generation; run with --write and inspect the diff",
    );
  }
}

const schema = JSON.parse(await readFile(schemaPath, "utf8"));
if (
  schema.$schema !== "https://json-schema.org/draft/2020-12/schema" ||
  schema.properties?.schema_version?.const !== "distill.projection-fixture/v1"
) {
  throw new Error("portable corpus schema is missing or version-mismatched");
}

const realSummary = verifyRealCorpus();
const realRejectionTests = runRealManifestRejectionTests();
const captureFailsClosed = runCaptureFailClosedTest();

console.log(
  JSON.stringify({
    mode: mode.slice(2),
    ...summary,
    secret_scan: "clean",
    rejection_tests: 5,
    oracle_self_test: "passed",
    real_corpus: realSummary,
    real_corpus_rejection_tests: realRejectionTests,
    real_corpus_capture_fails_closed: captureFailsClosed,
  }),
);

function runRejectionTests(fixture, profileMap) {
  const mutations = [
    (candidate) => {
      candidate.annotations.p0[0].needle_base64 = "YQ";
    },
    (candidate) => {
      candidate.source_sha256 = "0".repeat(64);
    },
    (candidate) => {
      delete candidate.annotations.p0[0].id;
    },
    (candidate) => {
      candidate.unknown_key = true;
    },
    (candidate) => {
      candidate.category = "unknown";
    },
  ];

  for (const mutate of mutations) {
    const candidate = structuredClone(fixture);
    mutate(candidate);
    let rejected = false;
    try {
      validateFixture(candidate, profileMap);
    } catch {
      rejected = true;
    }
    if (!rejected) {
      throw new Error("corpus validator accepted an incomplete fixture");
    }
  }
}

function runOracleTest(fixture) {
  const p0Needle = Buffer.from(fixture.annotations.p0[0].needle_base64, "base64");
  const score = scoreProjection(fixture, withOraclePrefix(p0Needle));
  if (score.p0.recall !== 1 || score.p1.recall !== 0) {
    throw new Error("byte-exact oracle self-test failed");
  }
}

function withOraclePrefix(bytes) {
  return Buffer.concat([Buffer.from("oracle:"), bytes]);
}

/**
 * The real corpus is captured, not generated: verification reads the committed
 * manifest and proves every stored fixture still matches its declared length,
 * digest, encoding, and scrub state.
 */
function verifyRealCorpus() {
  const records = parseRealManifest(readFileSync(realManifestPath, "utf8"));
  return validateRealCorpus(records, (path) => {
    const fixturePath = join(realDirectory, path);
    return existsSync(fixturePath) ? readFileSync(fixturePath) : null;
  });
}

function runRealManifestRejectionTests() {
  const records = parseRealManifest(readFileSync(realManifestPath, "utf8"));
  const readFixture = (path) => readFileSync(join(realDirectory, path));
  const mutations = [
    (candidate) => {
      candidate[0].sha256 = "0".repeat(64);
    },
    (candidate) => {
      candidate[0].byte_length += 1;
    },
    (candidate) => {
      candidate[0].shape = "unknown-shape";
    },
    (candidate) => {
      delete candidate[0].capture_host_class;
    },
    (candidate) => {
      candidate[1].id = candidate[0].id;
    },
    (candidate) => {
      candidate.length = 1;
    },
  ];

  for (const mutate of mutations) {
    const candidate = structuredClone(records);
    mutate(candidate);
    let rejected = false;
    try {
      validateRealCorpus(candidate, readFixture);
    } catch {
      rejected = true;
    }
    if (!rejected) {
      throw new Error("real corpus validator accepted an invalid manifest");
    }
  }
  return mutations.length;
}

/**
 * Proves the capture preflight fails closed: with every capture command hidden
 * from PATH, the runner names a missing command, writes no staging directory,
 * and leaves the committed manifest byte-identical.
 */
function runCaptureFailClosedTest() {
  const before = realSha256(readFileSync(realManifestPath));
  const emptyPath = mkdtempSync(join(tmpdir(), "distill-empty-path-"));
  let result;
  try {
    result = Bun.spawnSync({
      cmd: [process.execPath, realCapturePath],
      cwd: realDirectory,
      env: { PATH: emptyPath, HOME: process.env.HOME ?? emptyPath },
      stdout: "pipe",
      stderr: "pipe",
    });
  } finally {
    rmSync(emptyPath, { recursive: true, force: true });
  }

  const diagnostics = Buffer.from(result.stderr).toString("utf8");
  if (result.exitCode === 0) {
    throw new Error("real corpus capture succeeded without its capture commands");
  }
  if (!diagnostics.includes("capture command unavailable") || !diagnostics.includes("cargo")) {
    throw new Error("real corpus capture did not name its missing command");
  }
  const staging = readdirSync(realDirectory).filter((entry) => entry.startsWith(".staging-"));
  if (staging.length > 0) {
    throw new Error(`real corpus capture left partial staging directories: ${staging.join(",")}`);
  }
  if (realSha256(readFileSync(realManifestPath)) !== before) {
    throw new Error("real corpus capture rewrote the committed manifest while failing closed");
  }
  return "passed";
}
