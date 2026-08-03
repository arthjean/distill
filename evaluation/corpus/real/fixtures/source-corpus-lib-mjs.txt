import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

const FIXTURE_SCHEMA_VERSION = "distill.projection-fixture/v1";
const PROFILE_SCHEMA_VERSION = "distill.budget-profiles/v1";
const MAX_SOURCE_BYTES = 10 * 1024 * 1024;

const REQUIRED_CATEGORIES = [
  "build-output",
  "test-output",
  "logs",
  "diff",
  "diagnostics",
  "stack-trace",
  "source-code",
  "json",
  "empty",
  "unicode",
  "malformed-bytes",
  "prompt-injection",
];

const ALL_CATEGORIES = new Set([...REQUIRED_CATEGORIES, "boundary"]);
const TOP_LEVEL_KEYS = [
  "schema_version",
  "id",
  "category",
  "description",
  "reducible",
  "source",
  "source_sha256",
  "budget_profile",
  "expected",
  "annotations",
];
const FACT_ID = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const FIXTURE_ID = FACT_ID;
const SHA256 = /^[a-f0-9]{64}$/;
const textDecoder = new TextDecoder("utf-8", { fatal: true });

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

export function utf8(value) {
  return { kind: "utf8", value };
}

export function arbitrary(bytes) {
  return { kind: "base64", value: Buffer.from(bytes).toString("base64") };
}

export function padded(prefix, fillByte, byteLength) {
  return {
    kind: "padded",
    prefix_base64: Buffer.from(prefix).toString("base64"),
    fill_byte: fillByte,
    byte_length: byteLength,
  };
}

export function fact(id, needle) {
  return {
    id,
    needle_base64: Buffer.from(needle).toString("base64"),
  };
}

function materializePayload(payload) {
  if (!isRecord(payload)) {
    throw new Error("payload must be an object");
  }

  if (payload.kind === "utf8") {
    assertExactKeys(payload, ["kind", "value"], "utf8 payload");
    if (typeof payload.value !== "string") {
      throw new Error("utf8 payload value must be a string");
    }
    return Buffer.from(payload.value, "utf8");
  }

  if (payload.kind === "base64") {
    assertExactKeys(payload, ["kind", "value"], "base64 payload");
    if (typeof payload.value !== "string") {
      throw new Error("base64 payload value must be a string");
    }
    const bytes = Buffer.from(payload.value, "base64");
    if (bytes.toString("base64") !== payload.value) {
      throw new Error("base64 payload must use canonical encoding");
    }
    return bytes;
  }

  if (payload.kind === "padded") {
    assertExactKeys(
      payload,
      ["kind", "prefix_base64", "fill_byte", "byte_length"],
      "padded payload",
    );
    if (typeof payload.prefix_base64 !== "string") {
      throw new Error("padded prefix must be base64");
    }
    const prefix = Buffer.from(payload.prefix_base64, "base64");
    if (prefix.toString("base64") !== payload.prefix_base64) {
      throw new Error("padded prefix must use canonical base64");
    }
    if (
      !Number.isInteger(payload.fill_byte) ||
      payload.fill_byte < 0 ||
      payload.fill_byte > 255
    ) {
      throw new Error("padded fill_byte must be an octet");
    }
    if (
      !Number.isInteger(payload.byte_length) ||
      payload.byte_length < prefix.length ||
      payload.byte_length > MAX_SOURCE_BYTES
    ) {
      throw new Error("padded byte_length is outside the supported range");
    }
    return Buffer.concat([
      prefix,
      Buffer.alloc(payload.byte_length - prefix.length, payload.fill_byte),
    ]);
  }

  throw new Error(`unknown payload kind: ${String(payload.kind)}`);
}

export function materializeSource(source) {
  if (!isRecord(source)) {
    throw new Error("source must be an object");
  }

  if (source.kind === "inline") {
    assertExactKeys(source, ["kind", "payload"], "inline source");
    return materializePayload(source.payload);
  }

  if (source.kind === "file") {
    assertExactKeys(
      source,
      ["kind", "root_id", "relative_path", "payload"],
      "file source",
    );
    if (typeof source.root_id !== "string" || source.root_id.length === 0) {
      throw new Error("file root_id is required");
    }
    if (
      typeof source.relative_path !== "string" ||
      source.relative_path.length === 0 ||
      source.relative_path.startsWith("/") ||
      source.relative_path.split("/").some((part) => part === ".." || part === "")
    ) {
      throw new Error("file relative_path must stay beneath its root");
    }
    return materializePayload(source.payload);
  }

  if (source.kind === "process") {
    assertProcessSource(source);
    return Buffer.concat(source.events.map((event) => materializePayload(event.payload)));
  }

  throw new Error(`unknown source kind: ${String(source.kind)}`);
}

function assertProcessSource(source) {
  assertExactKeys(
    source,
    [
      "kind",
      "stdout",
      "stderr",
      "events",
      "exit_code",
      "signal",
      "timed_out",
      "working_directory",
      "truncated",
    ],
    "process source",
  );
  if (!Array.isArray(source.events)) {
    throw new Error("process events must be an array");
  }
  for (const [index, event] of source.events.entries()) {
    if (!isRecord(event)) {
      throw new Error(`process event ${index} must be an object`);
    }
    assertExactKeys(event, ["order", "stream", "payload"], `process event ${index}`);
    if (event.order !== index) {
      throw new Error("process event order must be contiguous and chronological");
    }
    if (event.stream !== "stdout" && event.stream !== "stderr") {
      throw new Error("process event stream must be stdout or stderr");
    }
    materializePayload(event.payload);
  }
  if (source.exit_code !== null && !Number.isInteger(source.exit_code)) {
    throw new Error("process exit_code must be an integer or null");
  }
  if (source.signal !== null && typeof source.signal !== "string") {
    throw new Error("process signal must be a string or null");
  }
  if (typeof source.timed_out !== "boolean" || typeof source.truncated !== "boolean") {
    throw new Error("process timeout and truncation flags must be booleans");
  }
  if (
    typeof source.working_directory !== "string" ||
    source.working_directory.length === 0
  ) {
    throw new Error("process working_directory is required");
  }

  const declaredStdout = materializePayload(source.stdout);
  const declaredStderr = materializePayload(source.stderr);
  const eventStdout = Buffer.concat(
    source.events
      .filter((event) => event.stream === "stdout")
      .map((event) => materializePayload(event.payload)),
  );
  const eventStderr = Buffer.concat(
    source.events
      .filter((event) => event.stream === "stderr")
      .map((event) => materializePayload(event.payload)),
  );
  if (!declaredStdout.equals(eventStdout) || !declaredStderr.equals(eventStderr)) {
    throw new Error("process stdout/stderr must equal their ordered event payloads");
  }
}

export function isValidUtf8(bytes) {
  try {
    textDecoder.decode(bytes);
    return true;
  } catch {
    return false;
  }
}

function expectedFor(source, category) {
  const bytes = materializeSource(source);
  const base = {
    source_kind: source.kind,
    content_class: category,
    byte_length: bytes.length,
    utf8_valid: isValidUtf8(bytes),
    complete: source.kind === "process" ? !source.truncated && !source.timed_out : true,
    truncated: source.kind === "process" ? source.truncated : false,
  };

  if (source.kind === "inline") {
    return {
      ...base,
      acquisition: {
        variant: "inline",
      },
    };
  }

  if (source.kind === "file") {
    return {
      ...base,
      acquisition: {
        variant: "file",
        root_id: source.root_id,
        relative_path: source.relative_path,
      },
    };
  }

  const stdout = materializePayload(source.stdout);
  const stderr = materializePayload(source.stderr);
  return {
    ...base,
    acquisition: {
      variant: "process",
      stdout_sha256: sha256(stdout),
      stderr_sha256: sha256(stderr),
      event_count: source.events.length,
      exit_code: source.exit_code,
      signal: source.signal,
      timed_out: source.timed_out,
      working_directory: source.working_directory,
      truncated: source.truncated,
    },
  };
}

export function makeFixture({
  id,
  category,
  description,
  reducible,
  source,
  budgetProfile,
  p0,
  p1,
}) {
  const bytes = materializeSource(source);
  return {
    schema_version: FIXTURE_SCHEMA_VERSION,
    id,
    category,
    description,
    reducible,
    source,
    source_sha256: sha256(bytes),
    budget_profile: budgetProfile,
    expected: expectedFor(source, category),
    annotations: {
      p0,
      p1,
    },
  };
}

export async function loadProfiles(path) {
  const document = JSON.parse(await readFile(path, "utf8"));
  if (!isRecord(document)) {
    throw new Error("budget profile document must be an object");
  }
  assertExactKeys(document, ["schema_version", "profiles"], "budget profile document");
  if (document.schema_version !== PROFILE_SCHEMA_VERSION) {
    throw new Error(`unsupported budget profile schema: ${String(document.schema_version)}`);
  }
  if (!Array.isArray(document.profiles) || document.profiles.length === 0) {
    throw new Error("at least one budget profile is required");
  }

  const profiles = new Map();
  for (const profile of document.profiles) {
    validateProfile(profile);
    if (profiles.has(profile.id)) {
      throw new Error(`duplicate budget profile: ${profile.id}`);
    }
    profiles.set(profile.id, profile);
  }
  return profiles;
}

function validateProfile(profile) {
  if (!isRecord(profile)) {
    throw new Error("budget profile must be an object");
  }
  const allowed =
    profile.unit === "tokens"
      ? [
          "id",
          "unit",
          "total_visible_limit",
          "reserved_envelope",
          "token_profile",
        ]
      : ["id", "unit", "total_visible_limit", "reserved_envelope"];
  assertExactKeys(profile, allowed, `budget profile ${String(profile.id)}`);
  if (typeof profile.id !== "string" || !FACT_ID.test(profile.id)) {
    throw new Error("budget profile id is invalid");
  }
  if (profile.unit !== "bytes" && profile.unit !== "tokens") {
    throw new Error(`budget profile ${profile.id} has an invalid unit`);
  }
  if (
    !Number.isInteger(profile.total_visible_limit) ||
    profile.total_visible_limit < 0 ||
    !Number.isInteger(profile.reserved_envelope) ||
    profile.reserved_envelope < 0 ||
    profile.reserved_envelope > profile.total_visible_limit
  ) {
    throw new Error(`budget profile ${profile.id} has invalid limits`);
  }
  if (
    profile.unit === "tokens" &&
    (typeof profile.token_profile !== "string" || profile.token_profile.length === 0)
  ) {
    throw new Error(`token budget ${profile.id} must name a token profile`);
  }
}

export function validateFixture(fixture, profiles) {
  if (!isRecord(fixture)) {
    throw new Error("fixture must be an object");
  }
  assertExactKeys(fixture, TOP_LEVEL_KEYS, `fixture ${String(fixture.id)}`);
  if (fixture.schema_version !== FIXTURE_SCHEMA_VERSION) {
    throw new Error(`fixture ${String(fixture.id)} has an unsupported schema`);
  }
  if (typeof fixture.id !== "string" || !FIXTURE_ID.test(fixture.id)) {
    throw new Error("fixture id is invalid");
  }
  if (!ALL_CATEGORIES.has(fixture.category)) {
    throw new Error(`fixture ${fixture.id} has an invalid category`);
  }
  if (
    typeof fixture.description !== "string" ||
    fixture.description.length === 0 ||
    fixture.description.length > 240
  ) {
    throw new Error(`fixture ${fixture.id} has an invalid description`);
  }
  if (typeof fixture.reducible !== "boolean") {
    throw new Error(`fixture ${fixture.id} reducible must be boolean`);
  }
  if (!profiles.has(fixture.budget_profile)) {
    throw new Error(`fixture ${fixture.id} references an unknown budget profile`);
  }
  if (typeof fixture.source_sha256 !== "string" || !SHA256.test(fixture.source_sha256)) {
    throw new Error(`fixture ${fixture.id} has an invalid source digest`);
  }

  const bytes = materializeSource(fixture.source);
  if (bytes.length > MAX_SOURCE_BYTES) {
    throw new Error(`fixture ${fixture.id} exceeds the 10 MiB source limit`);
  }
  if (sha256(bytes) !== fixture.source_sha256) {
    throw new Error(`fixture ${fixture.id} source digest does not match`);
  }

  const calculatedExpected = expectedFor(fixture.source, fixture.category);
  if (!deepEqual(fixture.expected, calculatedExpected)) {
    throw new Error(`fixture ${fixture.id} expected metadata is incomplete or incorrect`);
  }

  if (!isRecord(fixture.annotations)) {
    throw new Error(`fixture ${fixture.id} annotations must be an object`);
  }
  assertExactKeys(fixture.annotations, ["p0", "p1"], `fixture ${fixture.id} annotations`);
  if (!Array.isArray(fixture.annotations.p0) || !Array.isArray(fixture.annotations.p1)) {
    throw new Error(`fixture ${fixture.id} annotations must contain P0 and P1 arrays`);
  }
  if (
    fixture.reducible &&
    (fixture.annotations.p0.length === 0 || fixture.annotations.p1.length === 0)
  ) {
    throw new Error(`reducible fixture ${fixture.id} requires P0 and P1 facts`);
  }

  const factIds = new Set();
  for (const [priority, facts] of [
    ["p0", fixture.annotations.p0],
    ["p1", fixture.annotations.p1],
  ]) {
    for (const candidate of facts) {
      validateFact(candidate, fixture.id, priority, bytes, factIds);
    }
  }

  return {
    bytes,
    profile: profiles.get(fixture.budget_profile),
  };
}

function validateFact(candidate, fixtureId, priority, sourceBytes, factIds) {
  if (!isRecord(candidate)) {
    throw new Error(`fixture ${fixtureId} ${priority} fact must be an object`);
  }
  assertExactKeys(candidate, ["id", "needle_base64"], `fixture ${fixtureId} fact`);
  if (typeof candidate.id !== "string" || !FACT_ID.test(candidate.id)) {
    throw new Error(`fixture ${fixtureId} has an invalid fact id`);
  }
  if (factIds.has(candidate.id)) {
    throw new Error(`fixture ${fixtureId} repeats fact id ${candidate.id}`);
  }
  factIds.add(candidate.id);
  if (typeof candidate.needle_base64 !== "string") {
    throw new Error(`fixture ${fixtureId} fact ${candidate.id} lacks a byte needle`);
  }
  const needle = Buffer.from(candidate.needle_base64, "base64");
  if (
    needle.length === 0 ||
    needle.toString("base64") !== candidate.needle_base64 ||
    sourceBytes.indexOf(needle) === -1
  ) {
    throw new Error(`fixture ${fixtureId} fact ${candidate.id} is absent from source`);
  }
}

export function validateCorpus(fixtures, profiles) {
  if (!Array.isArray(fixtures) || fixtures.length < 100) {
    throw new Error("corpus must contain at least 100 fixtures");
  }
  const ids = new Set();
  const categories = new Map();
  let processFixtures = 0;

  for (const fixture of fixtures) {
    validateFixture(fixture, profiles);
    if (ids.has(fixture.id)) {
      throw new Error(`duplicate fixture id: ${fixture.id}`);
    }
    ids.add(fixture.id);
    categories.set(fixture.category, (categories.get(fixture.category) ?? 0) + 1);
    if (fixture.source.kind === "process") {
      processFixtures += 1;
    }
  }

  for (const category of REQUIRED_CATEGORIES) {
    if (!categories.has(category)) {
      throw new Error(`corpus is missing category ${category}`);
    }
  }
  if (processFixtures === 0) {
    throw new Error("corpus requires process fixtures");
  }

  const requiredBoundaries = new Map([
    ["boundary-zero-bytes", 0],
    ["boundary-one-byte", 1],
    ["boundary-exact-payload-budget", 224],
    ["boundary-one-over-payload-budget", 225],
    ["boundary-one-mib", 1024 * 1024],
    ["boundary-ten-mib", 10 * 1024 * 1024],
  ]);
  for (const [id, size] of requiredBoundaries) {
    const fixture = fixtures.find((candidate) => candidate.id === id);
    if (!fixture || fixture.expected.byte_length !== size) {
      throw new Error(`corpus boundary ${id} must contain exactly ${size} bytes`);
    }
  }

  return {
    fixtures: fixtures.length,
    categories: Object.fromEntries([...categories].sort(([a], [b]) => a.localeCompare(b))),
    process_fixtures: processFixtures,
  };
}

export function scoreProjection(fixture, visibleBytes) {
  const visible = Buffer.from(visibleBytes);
  const scoreClass = (facts) => {
    const details = facts.map((candidate) => {
      const needle = Buffer.from(candidate.needle_base64, "base64");
      return {
        id: candidate.id,
        recalled: visible.indexOf(needle) !== -1,
      };
    });
    const recalled = details.filter((candidate) => candidate.recalled).length;
    return {
      declared: details.length,
      recalled,
      recall: details.length === 0 ? 1 : recalled / details.length,
      details,
    };
  };
  return {
    p0: scoreClass(fixture.annotations.p0),
    p1: scoreClass(fixture.annotations.p1),
  };
}

export function scanSecrets(fixtures) {
  const rules = [
    {
      id: "private-key",
      pattern: /-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/u,
    },
    {
      id: "aws-access-key",
      pattern: /\bAKIA[0-9A-Z]{16}\b/u,
    },
    {
      id: "github-token",
      pattern: /\bgh[opsu]_[A-Za-z0-9]{36,}\b/u,
    },
    {
      id: "slack-token",
      pattern: /\bxox[baprs]-[A-Za-z0-9-]{20,}\b/u,
    },
    {
      id: "secret-assignment",
      pattern:
        /\b(?:api[_-]?key|secret|password|passwd|token)\s*[:=]\s*["']?[A-Za-z0-9+/=_-]{24,}/iu,
    },
  ];
  const findings = [];
  for (const fixture of fixtures) {
    const text = materializeSource(fixture.source).toString("utf8");
    for (const rule of rules) {
      if (rule.pattern.test(text)) {
        findings.push({ fixture: fixture.id, rule: rule.id });
      }
    }
  }
  return findings;
}

export async function readManifest(path) {
  const input = await readFile(path, "utf8");
  const fixtures = [];
  for (const [index, line] of input.split("\n").entries()) {
    if (line.length === 0) {
      continue;
    }
    try {
      fixtures.push(JSON.parse(line));
    } catch (error) {
      throw new Error(`manifest line ${index + 1} is invalid JSON: ${error.message}`);
    }
  }
  return fixtures;
}

export function serializeManifest(fixtures) {
  return `${fixtures.map((fixture) => JSON.stringify(fixture)).join("\n")}\n`;
}

export function percentile(sortedValues, fraction) {
  if (sortedValues.length === 0) {
    return null;
  }
  const index = Math.max(0, Math.ceil(sortedValues.length * fraction) - 1);
  return sortedValues[Math.min(index, sortedValues.length - 1)];
}

function assertExactKeys(value, expectedKeys, label) {
  const actual = Object.keys(value).sort();
  const expected = [...expectedKeys].sort();
  if (!deepEqual(actual, expected)) {
    throw new Error(
      `${label} keys differ: expected ${expected.join(",")}; got ${actual.join(",")}`,
    );
  }
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function deepEqual(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}
