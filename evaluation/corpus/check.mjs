import { readFile, writeFile } from "node:fs/promises";
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

const directory = dirname(fileURLToPath(import.meta.url));
const manifestPath = join(directory, "manifest.jsonl");
const profilesPath = join(directory, "budget-profiles.json");
const schemaPath = join(directory, "schema.json");

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

console.log(
  JSON.stringify({
    mode: mode.slice(2),
    ...summary,
    secret_scan: "clean",
    rejection_tests: 5,
    oracle_self_test: "passed",
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
