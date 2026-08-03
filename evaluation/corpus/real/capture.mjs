import {
  existsSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  boundedFixture,
  hostClass,
  isValidUtf8,
  makeRecord,
  normalize,
  normalizationRules,
  resolveExecutables,
  scanFixture,
  serializeRealManifest,
  validateRealCorpus,
} from "./lib.mjs";
import { SPECS, scaffold } from "./specs.mjs";

const directory = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(directory, "../../..");
const manifestPath = join(directory, "manifest.jsonl");
const fixturesPath = join(directory, "fixtures");

const home = homedir();
const workRoot = mkdtempSync(join(tmpdir(), "distill-real-corpus-"));
const captureHostClass = hostClass(process.platform, process.arch);
const rules = normalizationRules({
  home,
  repoRoot,
  workDir: workRoot,
  username: process.env.USER ?? process.env.LOGNAME ?? "",
  authorName: gitAuthorName(),
});

try {
  // Fails closed with the missing command named before any staging directory,
  // fixture, or manifest byte is written, and before the overwrite guard, so a
  // host missing a capture command can never consume the committed corpus.
  const executables = resolveExecutables(SPECS, (name) =>
    name.includes("/") ? existingPath(join(repoRoot, name)) : Bun.which(name),
  );
  if (existsSync(manifestPath) && !process.argv.includes("--force")) {
    throw new Error(
      "evaluation/corpus/real/manifest.jsonl already exists; re-capture requires --force",
    );
  }

  scaffold(workRoot);
  const staging = mkdtempSync(join(directory, ".staging-"));
  try {
    const records = SPECS.map((spec) => capture(spec, executables, staging));
    const summary = validateRealCorpus(records, (path) =>
      readIfPresent(join(staging, path.replace(/^fixtures\//u, ""))),
    );
    writeFileSync(join(staging, "manifest.jsonl"), serializeRealManifest(records), {
      encoding: "utf8",
      mode: 0o600,
    });
    rmSync(fixturesPath, { recursive: true, force: true });
    renameSync(join(staging, "manifest.jsonl"), manifestPath);
    renameSync(staging, fixturesPath);
    console.log(JSON.stringify({ mode: "capture", host_class: captureHostClass, ...summary }));
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw error;
  }
} finally {
  rmSync(workRoot, { recursive: true, force: true });
}

function capture(spec, executables, staging) {
  const cwd = spec.cwd === "repo" ? repoRoot : join(workRoot, spec.cwd);
  const argv = spec.command.map(expand);
  const result = Bun.spawnSync({
    cmd: [executables.get(spec.command[0]), ...argv.slice(1)],
    cwd,
    env: {
      PATH: process.env.PATH ?? "/usr/bin:/bin",
      HOME: home,
      LANG: "C",
      LC_ALL: "C",
      TZ: "UTC",
      TERM: "dumb",
      NO_COLOR: "1",
      CARGO_TERM_COLOR: "never",
      ...Object.fromEntries(
        Object.entries(spec.env ?? {}).map(([key, value]) => [key, expand(value)]),
      ),
    },
    stdout: "pipe",
    stderr: "pipe",
  });

  const stdout = normalizedBytes(result.stdout, spec.id, "stdout");
  const stderr = normalizedBytes(result.stderr, spec.id, "stderr");
  const captured = Buffer.concat([stdout, stderr]);
  if (captured.length === 0) {
    throw new Error(`real corpus fixture ${spec.id} captured no output`);
  }
  const findings = scanFixture(captured.toString("utf8"));
  if (findings.length > 0) {
    throw new Error(`real corpus fixture ${spec.id} failed the scrub scan: ${findings.join(",")}`);
  }

  const bounded = boundedFixture(captured);
  writeFileSync(join(staging, `${spec.id}.txt`), bounded.bytes, { mode: 0o600 });
  return makeRecord({
    id: spec.id,
    shape: spec.shape,
    description: spec.description,
    command: argv.map((part) => normalize(part, rules)),
    cwdLabel: spec.cwd === "repo" ? "repository-root" : `work/${spec.cwd}`,
    capture_host_class: captureHostClass,
    exitCode: result.exitCode,
    stdoutBytes: stdout.length,
    stderrBytes: stderr.length,
    bytes: bounded.bytes,
    truncated: bounded.truncated,
    sourceByteLength: bounded.sourceByteLength,
  });
}

function expand(part) {
  return part.replaceAll("{work}", workRoot);
}

/**
 * Decoding replaces invalid sequences, so a stream that is not valid UTF-8 fails
 * closed instead of being silently repaired into a fixture that then claims to
 * be valid. Byte-level observations stay the generated corpus's responsibility.
 */
function normalizedBytes(raw, id, stream) {
  const bytes = Buffer.from(raw);
  if (!isValidUtf8(bytes)) {
    throw new Error(`real corpus fixture ${id} captured ${stream} that is not valid UTF-8`);
  }
  return Buffer.from(normalize(bytes.toString("utf8"), rules), "utf8");
}

function gitAuthorName() {
  const git = Bun.which("git");
  if (git === null) {
    return "";
  }
  const result = Bun.spawnSync({ cmd: [git, "config", "user.name"], cwd: repoRoot });
  return result.exitCode === 0 ? Buffer.from(result.stdout).toString("utf8").trim() : "";
}

function existingPath(path) {
  return existsSync(path) ? path : null;
}

function readIfPresent(path) {
  return existsSync(path) ? readFileSync(path) : null;
}
