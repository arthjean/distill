import { createHash } from "node:crypto";

export const REAL_CORPUS_SCHEMA_VERSION = "distill.real-corpus/v1";
export const MAX_FIXTURE_BYTES = 256 * 1024;
export const MINIMUM_FIXTURES = 40;

export const SHAPES = [
  "build-output",
  "test-output",
  "typecheck-lint",
  "stack-trace",
  "unified-diff",
  "api-json",
  "source-file",
  "terminal-log",
];

export const PLACEHOLDER_HOME = "/home/dev";
export const PLACEHOLDER_REPO = "/home/dev/distill";
export const PLACEHOLDER_WORK = "/home/dev/work";
export const PLACEHOLDER_EMAIL = "dev@example.invalid";
export const PLACEHOLDER_USER = "dev";
export const PLACEHOLDER_AUTHOR = "Example Developer";

const RECORD_KEYS = [
  "schema_version",
  "id",
  "shape",
  "description",
  "command",
  "cwd_label",
  "capture_host_class",
  "exit_code",
  "stdout_bytes",
  "stderr_bytes",
  "byte_length",
  "line_count",
  "utf8_valid",
  "truncated",
  "source_byte_length",
  "sha256",
  "path",
];

const RECORD_ID = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const SHA256 = /^[a-f0-9]{64}$/;
const HOST_CLASS = /^[a-z0-9]+-[a-z0-9_]+$/;
const RELATIVE_PATH = /^fixtures\/[a-z0-9]+(?:-[a-z0-9]+)*\.txt$/;

const textDecoder = new TextDecoder("utf-8", { fatal: true });

const SCAN_RULES = [
  {
    id: "private-key",
    pattern: /-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/u,
  },
  { id: "aws-access-key", pattern: /\bAKIA[0-9A-Z]{16}\b/u },
  { id: "github-token", pattern: /\bgh[opsu]_[A-Za-z0-9]{36,}\b/u },
  { id: "slack-token", pattern: /\bxox[baprs]-[A-Za-z0-9-]{20,}\b/u },
  {
    id: "secret-assignment",
    pattern:
      /\b(?:api[_-]?key|secret|password|passwd|token)\s*[:=]\s*["']?[A-Za-z0-9+/=_-]{24,}/iu,
  },
  {
    id: "home-path",
    pattern: /\/(?:home|Users)\/(?!dev(?:[^A-Za-z0-9._-]|$))[A-Za-z0-9._-]+/u,
  },
  {
    id: "email",
    pattern:
      /(?<![A-Za-z0-9._%+-])(?!dev@example\.invalid\b)[A-Za-z0-9._%+-]+@[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}/u,
  },
];

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

const PLATFORM_CLASS = new Map([
  ["linux", "linux"],
  ["darwin", "macos"],
]);
const ARCHITECTURE_CLASS = new Map([
  ["x64", "x86_64"],
  ["arm64", "arm64"],
]);

/**
 * Coarse capture provenance: the qualified platform and architecture class, and
 * never a host name, user name, or any other host identity.
 */
export function hostClass(platform, architecture) {
  return `${PLATFORM_CLASS.get(platform) ?? platform}-${
    ARCHITECTURE_CLASS.get(architecture) ?? architecture
  }`;
}

/**
 * Ordered literal substitutions applied to captured bytes before digesting.
 * Longest host-specific paths are replaced first so nested paths cannot leave
 * a partially rewritten prefix behind.
 */
export function normalizationRules({ home, repoRoot, workDir, username, authorName }) {
  const rules = [
    { id: "work-directory", from: workDir, to: PLACEHOLDER_WORK },
    { id: "repository-root", from: repoRoot, to: PLACEHOLDER_REPO },
    { id: "home-directory", from: home, to: PLACEHOLDER_HOME },
  ].filter((rule) => typeof rule.from === "string" && rule.from.length > 0);
  rules.sort((left, right) => right.from.length - left.from.length);

  // Addresses are rewritten before names so a name that occurs inside an
  // address cannot leave a partially rewritten address behind.
  rules.push({
    id: "email-address",
    pattern:
      /[A-Za-z0-9._%+-]+@[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}/gu,
    to: PLACEHOLDER_EMAIL,
  });
  if (typeof authorName === "string" && authorName.length > 0) {
    rules.push({ id: "author-name", from: authorName, to: PLACEHOLDER_AUTHOR });
  }
  if (typeof username === "string" && username.length > 0) {
    rules.push({
      id: "username",
      pattern: new RegExp(`(?<![A-Za-z0-9_-])${escapeLiteral(username)}(?![A-Za-z0-9_-])`, "gu"),
      to: PLACEHOLDER_USER,
    });
  }
  return rules;
}

export function normalize(text, rules) {
  let normalized = text;
  for (const rule of rules) {
    normalized = rule.pattern
      ? normalized.replace(rule.pattern, rule.to)
      : normalized.replaceAll(rule.from, rule.to);
  }
  return normalized;
}

export function scanFixture(text) {
  return SCAN_RULES.filter((rule) => rule.pattern.test(text)).map((rule) => rule.id);
}

export function isValidUtf8(bytes) {
  try {
    textDecoder.decode(bytes);
    return true;
  } catch {
    return false;
  }
}

/**
 * Truncates on the last newline that keeps the fixture within the size ceiling
 * so a stored fixture never ends mid-line. Single-line output has no newline to
 * cut on, so it falls back to a conservative character boundary that may drop
 * one additional byte rather than risk a partial UTF-8 sequence.
 */
export function boundedFixture(bytes) {
  if (bytes.length <= MAX_FIXTURE_BYTES) {
    return { bytes, truncated: false, sourceByteLength: bytes.length };
  }
  const window = bytes.subarray(0, MAX_FIXTURE_BYTES);
  const newline = window.lastIndexOf(0x0a);
  const end = newline > 0 ? newline + 1 : boundaryBefore(window);
  return {
    bytes: bytes.subarray(0, end),
    truncated: true,
    sourceByteLength: bytes.length,
  };
}

function boundaryBefore(window) {
  let end = window.length;
  while (end > 0 && (window[end - 1] & 0b1100_0000) === 0b1000_0000) {
    end -= 1;
  }
  return end > 0 ? end - 1 : 0;
}

export function countLines(text) {
  if (text.length === 0) {
    return 0;
  }
  const lines = text.split("\n").length;
  return text.endsWith("\n") ? lines - 1 : lines;
}

export function makeRecord({
  id,
  shape,
  description,
  command,
  cwdLabel,
  capture_host_class,
  exitCode,
  stdoutBytes,
  stderrBytes,
  bytes,
  truncated,
  sourceByteLength,
}) {
  const text = new TextDecoder("utf-8").decode(bytes);
  return {
    schema_version: REAL_CORPUS_SCHEMA_VERSION,
    id,
    shape,
    description,
    command,
    cwd_label: cwdLabel,
    capture_host_class,
    exit_code: exitCode,
    stdout_bytes: stdoutBytes,
    stderr_bytes: stderrBytes,
    byte_length: bytes.length,
    line_count: countLines(text),
    utf8_valid: isValidUtf8(bytes),
    truncated,
    source_byte_length: sourceByteLength,
    sha256: sha256(bytes),
    path: `fixtures/${id}.txt`,
  };
}

export function validateRecord(record) {
  if (record === null || typeof record !== "object" || Array.isArray(record)) {
    return "record must be an object";
  }
  const actual = Object.keys(record).sort().join(",");
  const expected = [...RECORD_KEYS].sort().join(",");
  if (actual !== expected) {
    return `record keys differ: expected ${expected}; got ${actual}`;
  }
  if (record.schema_version !== REAL_CORPUS_SCHEMA_VERSION) {
    return `unsupported record schema: ${String(record.schema_version)}`;
  }
  if (typeof record.id !== "string" || !RECORD_ID.test(record.id)) {
    return "record id is invalid";
  }
  if (!SHAPES.includes(record.shape)) {
    return `record ${record.id} has an unknown shape`;
  }
  if (
    typeof record.description !== "string" ||
    record.description.length === 0 ||
    record.description.length > 240
  ) {
    return `record ${record.id} has an invalid description`;
  }
  if (
    !Array.isArray(record.command) ||
    record.command.length === 0 ||
    record.command.some((part) => typeof part !== "string" || part.length === 0)
  ) {
    return `record ${record.id} must name its originating command as literal argv`;
  }
  if (typeof record.cwd_label !== "string" || record.cwd_label.length === 0) {
    return `record ${record.id} must name its working directory class`;
  }
  if (
    typeof record.capture_host_class !== "string" ||
    !HOST_CLASS.test(record.capture_host_class)
  ) {
    return `record ${record.id} has an invalid capture host class`;
  }
  if (record.exit_code !== null && !Number.isInteger(record.exit_code)) {
    return `record ${record.id} exit code must be an integer or null`;
  }
  for (const key of ["stdout_bytes", "stderr_bytes", "byte_length", "line_count"]) {
    if (!Number.isInteger(record[key]) || record[key] < 0) {
      return `record ${record.id} field ${key} must be a non-negative integer`;
    }
  }
  if (record.stdout_bytes + record.stderr_bytes < record.byte_length) {
    return `record ${record.id} byte length exceeds its captured streams`;
  }
  if (record.byte_length > MAX_FIXTURE_BYTES) {
    return `record ${record.id} exceeds the ${MAX_FIXTURE_BYTES} byte fixture ceiling`;
  }
  if (typeof record.utf8_valid !== "boolean" || typeof record.truncated !== "boolean") {
    return `record ${record.id} state flags must be booleans`;
  }
  if (
    !Number.isInteger(record.source_byte_length) ||
    record.source_byte_length < record.byte_length ||
    record.truncated !== record.source_byte_length > record.byte_length
  ) {
    return `record ${record.id} truncation state disagrees with its byte lengths`;
  }
  if (typeof record.sha256 !== "string" || !SHA256.test(record.sha256)) {
    return `record ${record.id} has an invalid digest`;
  }
  if (record.path !== `fixtures/${record.id}.txt` || !RELATIVE_PATH.test(record.path)) {
    return `record ${record.id} has an invalid fixture path`;
  }
  return null;
}

/**
 * Validates the manifest as a whole against stored bytes. `readFixture` returns
 * the exact bytes committed for a record path, or null when it is absent.
 */
export function validateRealCorpus(records, readFixture) {
  if (!Array.isArray(records) || records.length < MINIMUM_FIXTURES) {
    throw new Error(
      `real corpus must contain at least ${MINIMUM_FIXTURES} fixtures; got ${
        Array.isArray(records) ? records.length : 0
      }`,
    );
  }
  const ids = new Set();
  const shapes = new Map();
  let digestsVerified = 0;

  for (const record of records) {
    const problem = validateRecord(record);
    if (problem !== null) {
      throw new Error(`real corpus manifest is invalid: ${problem}`);
    }
    if (ids.has(record.id)) {
      throw new Error(`duplicate real corpus fixture: ${record.id}`);
    }
    ids.add(record.id);
    shapes.set(record.shape, (shapes.get(record.shape) ?? 0) + 1);

    const bytes = readFixture(record.path);
    if (bytes === null) {
      throw new Error(`real corpus fixture ${record.id} is missing at ${record.path}`);
    }
    if (bytes.length !== record.byte_length) {
      throw new Error(
        `real corpus fixture ${record.id} is ${bytes.length} bytes; manifest declares ${record.byte_length}`,
      );
    }
    if (sha256(bytes) !== record.sha256) {
      throw new Error(`real corpus fixture ${record.id} digest does not match its manifest entry`);
    }
    digestsVerified += 1;
    if (isValidUtf8(bytes) !== record.utf8_valid) {
      throw new Error(`real corpus fixture ${record.id} UTF-8 validity disagrees with its manifest`);
    }
    const findings = scanFixture(new TextDecoder("utf-8").decode(bytes));
    if (findings.length > 0) {
      throw new Error(
        `real corpus fixture ${record.id} failed the scrub scan: ${findings.join(",")}`,
      );
    }
  }

  for (const shape of SHAPES) {
    if (!shapes.has(shape)) {
      throw new Error(`real corpus is missing shape ${shape}`);
    }
  }

  return {
    fixtures: records.length,
    digests_verified: digestsVerified,
    shapes: Object.fromEntries([...shapes].sort(([left], [right]) => left.localeCompare(right))),
    bytes: records.reduce((total, record) => total + record.byte_length, 0),
  };
}

/**
 * Fails closed with the missing executable named, before any capture runs and
 * before any staging directory or manifest is written.
 */
export function resolveExecutables(specs, which) {
  const resolved = new Map();
  const missing = [];
  for (const spec of specs) {
    const executable = spec.command[0];
    if (resolved.has(executable)) {
      continue;
    }
    const path = which(executable);
    if (typeof path !== "string" || path.length === 0) {
      missing.push(executable);
      continue;
    }
    resolved.set(executable, path);
  }
  if (missing.length > 0) {
    throw new Error(
      `real corpus capture command unavailable on this host: ${[...new Set(missing)].sort().join(", ")}`,
    );
  }
  return resolved;
}

export function serializeRealManifest(records) {
  return `${records.map((record) => JSON.stringify(record)).join("\n")}\n`;
}

export function parseRealManifest(text) {
  const records = [];
  for (const [index, line] of text.split("\n").entries()) {
    if (line.length === 0) {
      continue;
    }
    try {
      records.push(JSON.parse(line));
    } catch (error) {
      throw new Error(`real corpus manifest line ${index + 1} is invalid JSON: ${error.message}`);
    }
  }
  return records;
}

function escapeLiteral(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
}
